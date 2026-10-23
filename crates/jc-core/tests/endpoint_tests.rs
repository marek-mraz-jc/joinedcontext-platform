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
        (Representation::Csv, "csv"),
        (Representation::OgcFeatures, "ogc-features"),
        (Representation::Sta, "sta"),
        (Representation::Odata, "odata"),
        (Representation::WebSocket, "websocket"),
        (Representation::Ical, "ical"),
    ];

    let expected_names: Vec<&str> = representations.iter().map(|(_, name)| *name).collect();
    assert_eq!(
        expected_names,
        vec![
            "ngsi-ld",
            "mcp",
            "csv",
            "ogc-features",
            "sta",
            "odata",
            "websocket",
            "ical"
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
    empty_rep.enabled_representations = Some(vec![]);
    assert!(empty_rep.validate().is_err());

    // Duplicate representations fails
    let mut dup_rep = ep.spec.clone();
    dup_rep.enabled_representations = Some(vec![Representation::NgsiLd, Representation::NgsiLd]);
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

    // EP-117: a Policy URN outside the prefixed shape would bind no Policy, so it is refused
    let mut unprefixed = ep.spec.clone();
    unprefixed.policy_ref = Some(
        "urn:ngsi-ld:Policy:public-air-quality"
            .parse::<Urn>()
            .expect("valid URN"),
    );
    let refused = unprefixed.validate().expect_err("an unprefixed policyRef");
    assert!(refused.to_string().contains("policyRef"), "{refused}");
}

/// ADR-N-057 decision 2 (T-5037): `policyRef: X` reads as `policyRefs: [X]`; both together, or an
/// empty list, are refused, and every listed reference is checked as `policyRef` is.
#[test]
fn policy_refs_is_one_list_with_policy_ref() {
    let ep = Endpoint::from_yaml(GOLDEN).expect("valid golden YAML");
    let urn = |name: &str| {
        format!("urn:ngsi-ld:Policy:banskabystrica.sk:ovzdusie:{name}")
            .parse::<Urn>()
            .expect("valid URN")
    };
    assert_eq!(
        ep.spec.policy_refs(),
        Some(&[urn("public-air-quality")][..])
    );

    let mut listed = ep.spec.clone();
    listed.policy_ref = None;
    listed.policy_refs = Some(vec![urn("public-air-quality"), urn("sensors-write")]);
    listed.validate().expect("two Policies of the space");
    assert_eq!(listed.policy_refs().map(<[Urn]>::len), Some(2));

    let mut neither = listed.clone();
    neither.policy_refs = None;
    neither
        .validate()
        .expect("no reference binds the whole space");
    assert_eq!(neither.policy_refs(), None);

    let mut both = listed.clone();
    both.policy_ref = Some(urn("public-air-quality"));
    let refused = both.validate().expect_err("policyRef beside policyRefs");
    assert!(refused.to_string().contains("spec.policyRefs"), "{refused}");

    let mut empty = listed.clone();
    empty.policy_refs = Some(Vec::new());
    let refused = empty.validate().expect_err("an empty policyRefs");
    assert!(
        refused.to_string().contains("at least one Policy"),
        "{refused}"
    );

    let mut unprefixed = listed.clone();
    unprefixed.policy_refs = Some(vec![
        urn("public-air-quality"),
        "urn:ngsi-ld:Policy:loose"
            .parse::<Urn>()
            .expect("valid URN"),
    ]);
    unprefixed
        .validate()
        .expect_err("an unprefixed entry of policyRefs");

    let wire = serde_json::to_value(&listed).expect("serializes");
    assert_eq!(wire["policyRefs"][1], urn("sensors-write").to_string());
    assert!(wire.get("policyRef").is_none());
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

fn listing(representations: &str, extra: &str) -> Endpoint {
    Endpoint::from_yaml(&format!(
        "apiVersion: joinedcontext.com/v1alpha1\nkind: Endpoint\nmetadata:\n  name: air\n  \
         namespace: bb-ovzdusie\nspec:\n  contextSpaceRef: ovzdusie\n  \
         slug: zt4qm7ge2xdv6ksb3ncf5arw2y\n  audience: project-list\n  \
         allowedProjects: [bb-doprava]\n  enabledRepresentations: {representations}\n{extra}"
    ))
    .expect("the manifest parses")
}

/// EP-24: MCP is served without being listed, internal or public, and only `mcp: false`
/// leaves it out; the listed order stays as written.
#[test]
fn every_endpoint_serves_mcp_unless_it_opts_out() {
    let listed = listing("[ngsi-ld, csv]", "");
    listed.validate().expect("valid");
    assert_eq!(
        listed.spec.served_representations(),
        [
            Representation::NgsiLd,
            Representation::Csv,
            Representation::Mcp
        ]
    );

    let already = listing("[mcp, ngsi-ld]", "");
    assert_eq!(
        already.spec.served_representations(),
        [Representation::Mcp, Representation::NgsiLd],
        "a listed mcp is not served twice"
    );

    let off = listing("[ngsi-ld]", "  mcp: false\n");
    off.validate().expect("an opt-out is valid");
    assert_eq!(off.spec.served_representations(), [Representation::NgsiLd]);

    let on = listing("[ngsi-ld]", "  mcp: true\n");
    assert!(on
        .spec
        .served_representations()
        .contains(&Representation::Mcp));
}

/// EP-10 (T-2939, T-4597): NGSI-LD is served without being listed, like MCP, so an endpoint
/// that lists only its file still serves the uniform surface, maps included (geo+json).
#[test]
fn every_endpoint_serves_ngsi_ld_whatever_it_lists() {
    let files = listing("[csv]", "");
    files.validate().expect("valid");
    assert_eq!(
        files.spec.served_representations(),
        [
            Representation::Csv,
            Representation::NgsiLd,
            Representation::Mcp
        ]
    );
    let each_once = listing("[ngsi-ld, mcp]", "");
    assert_eq!(
        each_once.spec.served_representations(),
        [Representation::NgsiLd, Representation::Mcp]
    );
}

/// T-4597 (API/02 §5): a manifest that still names a removed file is refused with a sentence
/// that names the removal and the way out, in either list, not as an unknown word.
#[test]
fn a_removed_representation_is_refused_by_name() {
    for removed in ["geojson", "json", "xlsx", "parquet", "zip"] {
        for list in ["enabledRepresentations", "disabledRepresentations"] {
            let raw = format!(
                "apiVersion: joinedcontext.com/v1alpha1\nkind: Endpoint\nmetadata:\n  name: air\n  \
                 namespace: bb-ovzdusie\nspec:\n  contextSpaceRef: ovzdusie\n  \
                 slug: zt4qm7ge2xdv6ksb3ncf5arw2y\n  audience: public\n  {list}: [{removed}]\n"
            );
            let refused = Endpoint::from_yaml(&raw).expect_err("a removed file is refused");
            let message = refused.to_string();
            assert!(
                message.contains(&format!("representation `{removed}` was removed"))
                    && message.contains("file.csv"),
                "{list}: {message}"
            );
        }
    }
    let typo = Endpoint::from_yaml(
        "apiVersion: joinedcontext.com/v1alpha1\nkind: Endpoint\nmetadata:\n  name: air\n  \
         namespace: bb-ovzdusie\nspec:\n  contextSpaceRef: ovzdusie\n  \
         slug: zt4qm7ge2xdv6ksb3ncf5arw2y\n  audience: public\n  enabledRepresentations: [cvs]\n",
    )
    .expect_err("an unknown name is refused");
    assert!(
        typo.to_string().contains("unknown variant `cvs`")
            && !typo.to_string().contains("was removed"),
        "{typo}"
    );
}

/// T-4597: the Portal's API hands the enum JSON, where a name may arrive escaped and so not
/// borrowed from the input; it is refused by the same name, and a removed name in another case
/// is just unknown.
#[test]
fn a_removed_representation_in_json_is_refused_by_name_however_it_is_written() {
    let escaped = serde_json::from_str::<Representation>(r#""\u0078lsx""#)
        .expect_err("an escaped removed name is refused");
    assert!(
        escaped
            .to_string()
            .contains("representation `xlsx` was removed"),
        "{escaped}"
    );
    let shouted =
        serde_json::from_str::<Representation>(r#""XLSX""#).expect_err("names are lower case");
    assert!(
        shouted.to_string().contains("unknown variant `XLSX`"),
        "{shouted}"
    );
    assert_eq!(
        serde_json::from_str::<Representation>(r#""\u0063sv""#).expect("an escaped csv"),
        Representation::Csv
    );
}

/// EP-24: `mcp: false` beside a listed `mcp` says two things at once and is refused, with the
/// way out in the message.
#[test]
fn an_opt_out_beside_a_listed_mcp_is_refused() {
    let both = listing("[ngsi-ld, mcp]", "  mcp: false\n");
    let refused = both.validate().expect_err("a contradiction is refused");
    assert!(
        refused.to_string().contains("drop one of the two"),
        "{refused}"
    );
}

/// The opt-out is a boolean; anything else is refused at parse time, not read as "on".
#[test]
fn a_non_boolean_mcp_switch_is_refused() {
    let raw = "apiVersion: joinedcontext.com/v1alpha1\nkind: Endpoint\nmetadata:\n  name: air\n  \
               namespace: bb-ovzdusie\nspec:\n  contextSpaceRef: ovzdusie\n  \
               slug: zt4qm7ge2xdv6ksb3ncf5arw2y\n  audience: public\n  \
               enabledRepresentations: [ngsi-ld]\n  mcp: \"off\"\n";
    assert!(Endpoint::from_yaml(raw).is_err());
}

#[test]
fn endpoint_creates_parses_and_bounds_per_day() {
    let ep = Endpoint::from_yaml(GOLDEN).expect("valid golden YAML");
    let yaml = "mintIds: true\nperDay: 200\n";
    let creates: jc_core::kinds::Creates = serde_norway::from_str(yaml).expect("creates parses");
    assert!(creates.mint_ids);
    assert_eq!(creates.per_day, Some(200));
    assert!(
        serde_norway::from_str::<jc_core::kinds::Creates>("mintIds: true\nperHour: 3\n").is_err()
    );

    for (per_day, ok) in [
        (Some(1), true),
        (Some(10_000), true),
        (None, true),
        (Some(0), false),
        (Some(10_001), false),
    ] {
        let mut spec = ep.spec.clone();
        spec.creates = Some(jc_core::kinds::Creates {
            mint_ids: true,
            per_day,
            ..Default::default()
        });
        assert_eq!(spec.validate().is_ok(), ok, "perDay {per_day:?}");
    }
}

/// EP-101 (T-3266): the sites that may frame a public form are https origins, at most 20, each
/// written once and exactly as a browser names it, because the Portal sends them as they are in a
/// `frame-ancestors` header.
#[test]
fn a_form_names_the_sites_that_may_frame_it_as_https_origins() {
    let ep = Endpoint::from_yaml(GOLDEN).expect("valid golden YAML");
    let read: jc_core::kinds::Creates = serde_norway::from_str(
        "mintIds: true\nembedOrigins: [https://www.banskabystrica.sk, https://forms.example.org:8443]\n",
    )
    .expect("creates parses");
    assert_eq!(
        read.embed_origins,
        [
            "https://www.banskabystrica.sk",
            "https://forms.example.org:8443"
        ]
    );
    let with = |origins: Vec<String>| {
        let mut spec = ep.spec.clone();
        spec.creates = Some(jc_core::kinds::Creates {
            mint_ids: true,
            embed_origins: origins,
            ..Default::default()
        });
        spec.validate()
    };
    assert!(
        with(Vec::new()).is_ok(),
        "no list: only the Portal frames it"
    );
    assert!(with(vec!["https://www.banskabystrica.sk".into()]).is_ok());
    for refused in [
        "http://www.banskabystrica.sk",
        "https://www.banskabystrica.sk/",
        "https://www.banskabystrica.sk/forms",
        "https://www.banskabystrica.sk?x=1",
        "https://*.banskabystrica.sk",
        "*",
        "https://user:pw@banskabystrica.sk",
        "https://WWW.BanskaBystrica.sk",
        "https://banská.sk",
        "https://a;b.sk",
        "https://-a.sk",
        "https://banskabystrica.sk:443",
        "https://banskabystrica.sk 'unsafe-inline'",
        "",
    ] {
        let err = with(vec![refused.into()]).expect_err(refused);
        assert!(
            err.to_string().contains("creates.embedOrigins"),
            "{refused}: {err}"
        );
    }
    let twice = vec!["https://a.sk".to_owned(), "https://a.sk".to_owned()];
    assert!(with(twice).is_err(), "an origin written twice");
    let many: Vec<String> = (0..21)
        .map(|n| format!("https://s{n}.example.org"))
        .collect();
    assert!(with(many[..20].to_vec()).is_ok());
    assert!(with(many).is_err(), "at most 20");
}

/// The golden Endpoint without `enabledRepresentations`, plus the given `spec` lines.
fn defaulted(extra: &str) -> Result<Endpoint, String> {
    let without: String = GOLDEN
        .lines()
        .filter(|l| !l.starts_with("  enabledRepresentations:") && !l.starts_with("    - "))
        .map(|l| format!("{l}\n"))
        .collect();
    Endpoint::from_yaml(&format!("{without}{extra}")).map_err(|e| e.to_string())
}

#[test]
fn an_endpoint_that_lists_nothing_serves_every_standard_but_sta() {
    let ep = defaulted("").expect("parses");
    ep.validate().expect("validates");
    use Representation as R;
    assert_eq!(
        ep.spec.served_representations(),
        [
            R::NgsiLd,
            R::Mcp,
            R::Csv,
            R::OgcFeatures,
            R::Odata,
            R::WebSocket
        ],
        "EP-104, EP-125: SensorThings and iCalendar are opt-in"
    );
    let narrowed =
        defaulted("  disabledRepresentations: [odata, csv, websocket]\n").expect("parses");
    narrowed.validate().expect("validates");
    assert_eq!(
        narrowed.spec.served_representations(),
        [R::NgsiLd, R::Mcp, R::OgcFeatures]
    );
    let no_mcp = defaulted("  mcp: false\n").expect("parses");
    no_mcp.validate().expect("validates");
    assert!(!no_mcp.spec.served_representations().contains(&R::Mcp));
    let round = Endpoint::from_yaml(&narrowed.to_yaml().expect("yaml")).expect("re-import");
    assert_eq!(round, narrowed, "an omitted list stays omitted");
    assert!(!narrowed
        .to_yaml()
        .expect("yaml")
        .contains("enabledRepresentations"));
}

#[test]
fn an_explicit_list_serves_what_it_served_before() {
    let ep = Endpoint::from_yaml(GOLDEN).expect("golden");
    use Representation as R;
    assert_eq!(
        ep.spec.served_representations(),
        [R::NgsiLd, R::Mcp, R::Csv, R::OgcFeatures, R::Sta],
        "the list, and nothing the default would add"
    );
}

#[test]
fn disabled_representations_are_checked() {
    for (extra, why) in [
        (
            "  disabledRepresentations: [ngsi-ld]\n",
            "ngsi-ld is always served",
        ),
        (
            "  disabledRepresentations: [mcp]\n",
            "mcp: false is the switch",
        ),
        ("  disabledRepresentations: [csv, csv]\n", "a duplicate"),
    ] {
        let ep = defaulted(extra).expect("parses");
        assert!(ep.validate().is_err(), "{why}");
    }
    let mut beside = Endpoint::from_yaml(GOLDEN).expect("golden");
    beside.spec.disabled_representations = vec![Representation::Csv];
    assert!(beside.validate().is_err(), "beside an explicit list");
    assert!(
        defaulted("  disabledRepresentations: [graphql]\n").is_err(),
        "an unknown name"
    );
    assert!(
        defaulted("  disabledRepresentation: [csv]\n").is_err(),
        "an unknown field (deny_unknown_fields)"
    );
}

/// EP-125: `ical` checks its mapping and `sta` its fixed requirement against the model; the
/// generic ones serve any type.
#[test]
fn only_ical_and_sta_need_the_model() {
    use Representation as R;
    for r in R::ALL {
        assert_eq!(r.needs_model(), matches!(r, R::Ical | R::Sta), "{r}");
        if !matches!(r, R::Sta) {
            assert_eq!(r.requirement(), None, "{r}");
            assert_eq!(r.model_gap("classes: {}").unwrap(), None, "{r}");
        }
    }
}

/// The RFC 5545 sections that define each field and, for UID, DTSTAMP and DTSTART, require it.
fn section(field: &str) -> &'static str {
    match field {
        "UID" => "§3.6.1, §3.8.4.7",
        "DTSTAMP" => "§3.6.1, §3.8.7.2",
        "DTSTART" => "§3.6.1, §3.8.2.4",
        "DTEND" => "§3.8.2.2",
        "DURATION" => "§3.8.2.5",
        "SUMMARY" => "§3.8.1.12",
        "DESCRIPTION" => "§3.8.1.5",
        "LOCATION" => "§3.8.1.7",
        "GEO" => "§3.8.1.6",
        "URL" => "§3.8.4.6",
        "CATEGORIES" => "§3.8.1.2",
        "STATUS" => "§3.8.1.11",
        "ORGANIZER" => "§3.8.4.3",
        "LAST-MODIFIED" => "§3.8.7.3",
        "CLASS" => "§3.8.1.3",
        other => panic!("no RFC 5545 section for {other}"),
    }
}

/// The iCalendar fields, remaps and presets as the Markdown `schemas/surface-requirements.md`
/// commits and API/02 §8b quotes (EP-125).
fn markdown() -> String {
    use jc_core::kinds::{FieldSource, IcalField, Preset};
    let mut out = String::from(
        "# Domain-standard representations and their mappings\n\n\
         Generated from `jc_core::kinds::ical_mapping` (jc-core); a test fails when this file \
         and the code differ. `UPDATE_SCHEMAS=1 cargo test -p jc-core --test endpoint_tests` \
         rewrites it. An Endpoint that serves a representation below maps its fields from \
         NGSI-LD attributes in `spec.representations.<name>`; `jcctl validate` and the Portal \
         check the mapping against the space's data model (EP-125, API/02 §8b).\n\n\
         ## `ical` (RFC 5545)\n\n\
         | Field | Section | Required | Source accepts | Remaps |\n|---|---|---|---|---|\n",
    );
    for field in IcalField::ALL {
        let required = match field {
            IcalField::Uid => "yes, fixed: entity `id`",
            IcalField::Dtstamp => "yes, fixed: entity `modifiedAt`",
            f if f.required() => "yes",
            _ => "no",
        };
        let remaps = field.remaps();
        out.push_str(&format!(
            "| `{field}` | {} | {required} | {} | {} |\n",
            section(field.as_str()),
            field.accepts().words(),
            if remaps.is_empty() {
                "none".to_owned()
            } else {
                remaps
                    .iter()
                    .map(|r| format!("`{r}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        ));
    }
    for preset in Preset::ALL {
        out.push_str(&format!(
            "\n### Preset `{}`\n\n| Field | Source |\n|---|---|\n",
            preset.as_str()
        ));
        for (field, source) in preset.mapping() {
            let source = match source {
                FieldSource::Attribute(name) => format!("`{name}`"),
                FieldSource::Remap(remap) => format!(
                    "`{}`, mapped ({} values), default `{}`",
                    remap.attribute.unwrap_or_default(),
                    remap.map.len(),
                    remap.default.unwrap_or_default()
                ),
            };
            out.push_str(&format!("| `{field}` | {source} |\n"));
        }
    }
    for representation in Representation::ALL {
        let Some(need) = representation.requirement() else {
            continue;
        };
        let kind = match need.entity_type {
            jc_core::kinds::endpoint::ANY_TYPE => {
                "any type with a slot of numeric `range` that declares its `unit`".to_owned()
            }
            named => format!("`{named}`"),
        };
        out.push_str(&format!(
            "\n## `{representation}` ({})\n\nA fixed requirement: the space's model needs {kind} ({}).\n\n| Field | Section | Source attribute | Mandatory |\n|---|---|---|---|\n",
            need.standard, need.type_iri
        ));
        for (field, section, attribute, mandatory) in need.fields {
            out.push_str(&format!(
                "| `{field}` | {section} | `{attribute}` | {} |\n",
                if *mandatory { "yes" } else { "no" }
            ));
        }
    }
    out
}

/// The committed `schemas/surface-requirements.md` is what the code declares (EP-125, T-4573).
/// `UPDATE_SCHEMAS=1` writes it.
#[test]
fn the_committed_surface_requirements_are_the_generated_ones() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root")
        .join("schemas/surface-requirements.md");
    let generated = markdown();
    if std::env::var("UPDATE_SCHEMAS").is_ok() {
        std::fs::write(&path, &generated).expect("written");
    }
    let committed = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{}: {e} — run with UPDATE_SCHEMAS=1", path.display()));
    assert_eq!(committed, generated, "{} is stale", path.display());
    assert!(committed.contains("| `DTSTART` | §3.6.1, §3.8.2.4 | yes | date or datetime |"));
}

/// EP-125 (T-4886): `sta` serves any type of the model with a measurement, a numeric slot that
/// declares its unit; anything else, an unreadable model included, is named.
#[test]
fn sta_needs_a_numeric_slot_with_its_unit_in_some_class() {
    use Representation as R;
    let gap = Some("a type with a numeric attribute that declares its unit");
    let cases = [
        // A global slot with range and unit, listed by the class.
        ("slots:\n  pm10: { range: float, unit: { ucum_code: ug/m3 } }\nclasses:\n  Air:\n    slots: [pm10]\n", None),
        // The class's slot_usage gives the unit to a numeric global slot.
        ("slots:\n  speed: { range: double }\nclasses:\n  Vehicle:\n    slots: [speed]\n    slot_usage:\n      speed: { unit: { ucum_code: km/h } }\n", None),
        // An attribute of the class.
        ("classes:\n  Meter:\n    attributes:\n      reading: { range: integer, unit: { symbol: kWh } }\n", None),
        // A number without a unit.
        ("classes:\n  Article:\n    attributes:\n      wordCount: { range: integer }\n", gap),
        // A unit on a text.
        ("classes:\n  Note:\n    attributes:\n      text: { range: string, unit: { symbol: m } }\n", gap),
        // A measured slot no class lists.
        ("slots:\n  pm10: { range: float, unit: { ucum_code: ug/m3 } }\nclasses:\n  Air:\n    slots: [name]\n", gap),
        ("classes: {}", gap),
    ];
    for (linkml, want) in cases {
        assert_eq!(
            R::Sta.model_gap(linkml).unwrap().as_deref(),
            want,
            "{linkml}"
        );
    }
    assert!(R::Sta.model_gap("classes: [").is_err());
    assert_eq!(R::Sta.missing_model_gap().as_deref(), gap);
    // `ical` has no fixed requirement: its mapping names the type (ical_mapping_tests).
    assert_eq!(R::Ical.missing_model_gap(), None);
    assert_eq!(R::Odata.missing_model_gap(), None);
}
