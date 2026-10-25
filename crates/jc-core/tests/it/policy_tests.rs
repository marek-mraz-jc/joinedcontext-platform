use jc_core::kinds::policy::{
    Operation, OperationGroup, OperationRef, Principal, PrincipalKind, ScopeDefinitionSpec,
    Validity,
};
use jc_core::kinds::{Policy, ScopeDefinition};
use regex::Regex;

const GOLDEN_POLICY: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: Policy
metadata:
  name: public-air-quality
  namespace: ovzdusie
  title:
    sk: "Verejné dáta o ovzduší"
    en: "Public air-quality data"
spec:
  contextSpaceRef:
    kind: ContextSpace
    name: ovzdusie
  assigner: "did:web:banskabystrica.sk"
  assignee:
    kind: role
    id: public
  operations:
    - queryEntity
    - retrieveEntity
    - queryTemporal
  information:
    - entities:
        - type: AirQualityObserved
          idPattern: "^urn:ngsi-ld:AirQualityObserved:banskabystrica\\.sk:ovzdusie:.*$"
      propertyNames:
        - pm10
        - pm25
        - dateObserved
        - location
      relationshipNames:
        - refDistrict
  q: "pm10>=0"
  scopeQ: "/geo/SK/BB"
  geoQ: "georel=within;geometry=Polygon;coordinates=[[[19.10,48.70],[19.20,48.70],[19.20,48.76],[19.10,48.76],[19.10,48.70]]]"
  temporalQ: "timerel=after;timeAt=P-1D"
  validity:
    from: "2026-09-01T00:00:00Z"
    to: "2027-09-01T00:00:00Z"
"#;

const GOLDEN_SCOPE: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: ScopeDefinition
metadata:
  name: geo-sk-bb-radvan
  namespace: ovzdusie
  title:
    sk: "Radvaň"
    en: "Radvan"
spec:
  scopeString: /geo/SK/BB/Radvan
  isRoot: false
  isChildOf: /geo/SK/BB
"#;

#[test]
fn golden_policy_parses_validates_and_roundtrips() {
    let p = Policy::from_yaml(GOLDEN_POLICY).expect("valid golden YAML");
    p.validate().expect("golden policy validates");

    assert_eq!(
        p.resource_path().expect("resource path"),
        "projects/ovzdusie/spaces/ovzdusie/policies/public-air-quality.yaml"
    );

    let serialized = p.to_yaml().expect("serialize to yaml");
    let reimported = Policy::from_yaml(&serialized).expect("re-import yaml");
    assert_eq!(p, reimported);
}

#[test]
fn operations_vocabulary_roundtrips_and_rejects_proprietary_verbs_r8() {
    assert!(serde_json::from_str::<OperationRef>("\"upsertEntity\"").is_err());
    assert!(serde_json::from_str::<OperationRef>("\"read\"").is_err());
    assert!(serde_json::from_str::<OperationRef>("\"WRITE\"").is_err());

    // 43 individual operations in Table 4.20-1 order
    let ops = [
        "createEntity",
        "updateEntity",
        "appendAttrs",
        "updateAttrs",
        "deleteAttrs",
        "deleteEntity",
        "createBatch",
        "upsertBatch",
        "updateBatch",
        "deleteBatch",
        "upsertTemporal",
        "appendAttrsTemporal",
        "deleteAttrsTemporal",
        "updateAttrInstanceTemporal",
        "deleteAttrInstanceTemporal",
        "deleteTemporal",
        "mergeEntity",
        "replaceEntity",
        "replaceAttrs",
        "mergeBatch",
        "purgeEntity",
        "retrieveEntity",
        "queryEntity",
        "queryBatch",
        "retrieveTemporal",
        "queryTemporal",
        "retrieveEntityTypes",
        "retrieveEntityTypeDetails",
        "retrieveEntityTypeInfo",
        "retrieveAttrTypes",
        "retrieveAttrTypeDetails",
        "retrieveAttrTypeInfo",
        "createSubscription",
        "updateSubscription",
        "retrieveSubscription",
        "querySubscription",
        "deleteSubscription",
        "retrieveEntityMap",
        "updateEntityMap",
        "deleteEntityMap",
        "createEntityMapQueryEntity",
        "createEntityMapQueryTemporal",
        "retrieveContextSourceIdentity",
    ];

    // 5 group names in Table 4.20-2 order
    let groups = [
        "federationOps",
        "associationOps",
        "updateOps",
        "retrieveOps",
        "redirectionOps",
    ];

    assert_eq!(ops.len(), 43);
    assert_eq!(groups.len(), 5);

    for name in ops.iter().chain(groups.iter()) {
        let json = format!("\"{name}\"");
        let parsed: OperationRef = serde_json::from_str(&json).expect("valid operation");
        assert_eq!(parsed.as_str(), *name);
        let serialized = serde_json::to_string(&parsed).expect("serialize operation");
        assert_eq!(serialized, json);
    }
}

#[test]
fn operation_is_write_classification() {
    let writes = [
        Operation::CreateEntity,
        Operation::UpdateAttrs,
        Operation::DeleteEntity,
        Operation::UpsertBatch,
        Operation::MergeEntity,
        Operation::PurgeEntity,
        Operation::AppendAttrsTemporal,
        Operation::CreateSubscription,
        Operation::DeleteSubscription,
    ];
    for op in writes {
        assert!(op.is_write(), "{:?} should be classified as write", op);
    }

    let reads = [
        Operation::QueryEntity,
        Operation::RetrieveEntity,
        Operation::QueryTemporal,
        Operation::RetrieveEntityTypes,
        Operation::RetrieveSubscription,
        Operation::QuerySubscription,
        Operation::RetrieveEntityMap,
        Operation::RetrieveContextSourceIdentity,
    ];
    for op in reads {
        assert!(!op.is_write(), "{:?} should be classified as read", op);
    }
}

#[test]
fn id_pattern_anchoring_and_compilation_r24_r33() {
    let p = Policy::from_yaml(GOLDEN_POLICY).expect("valid golden YAML");

    // Unanchored pattern -> rejected
    let mut unanchored = p.spec.clone();
    unanchored.information[0].entities[0].id_pattern =
        Some("urn:ngsi-ld:AirQualityObserved:.*".to_string());
    assert!(unanchored.validate().is_err());

    // Anchored only at start -> rejected
    let mut start_only = p.spec.clone();
    start_only.information[0].entities[0].id_pattern =
        Some("^urn:ngsi-ld:AirQualityObserved:.*".to_string());
    assert!(start_only.validate().is_err());

    // Invalid regex syntax -> rejected
    let mut bad_regex = p.spec.clone();
    bad_regex.information[0].entities[0].id_pattern = Some("^([a-$".to_string());
    assert!(bad_regex.validate().is_err());

    // Golden anchored regex compiles and matches local tenant URN, rejects foreign domain
    let golden_pat = p.spec.information[0].entities[0]
        .id_pattern
        .as_ref()
        .expect("has pattern");
    let re = Regex::new(golden_pat).expect("golden pattern compiles");
    assert!(
        re.is_match("urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:ovzdusie:station-radvan-01")
    );
    assert!(!re.is_match("urn:ngsi-ld:AirQualityObserved:other-city.sk:ovzdusie:station-radvan-01"));
}

#[test]
fn policy_selector_and_spec_constraints() {
    let p = Policy::from_yaml(GOLDEN_POLICY).expect("valid golden YAML");

    // id and idPattern mutually exclusive
    let mut both = p.spec.clone();
    both.information[0].entities[0].id = Some(
        "urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:ovzdusie:station-01"
            .parse()
            .unwrap(),
    );
    assert!(both.validate().is_err());

    // Non-PascalCase entity type fails
    let mut bad_type = p.spec.clone();
    bad_type.information[0].entities[0].entity_type = "airQuality".to_string();
    assert!(bad_type.validate().is_err());

    // validity from > to fails
    let mut bad_val = p.spec.clone();
    bad_val.validity = Some(Validity {
        from: Some("2027-01-01T00:00:00Z".parse().unwrap()),
        to: Some("2026-01-01T00:00:00Z".parse().unwrap()),
    });
    assert!(bad_val.validate().is_err());

    // has_write on golden is false; when updateEntity added is true
    assert!(!p.spec.has_write());
    let mut with_write = p.spec.clone();
    with_write
        .operations
        .push(OperationRef::Single(Operation::UpdateEntity));
    assert!(with_write.has_write());
}

#[test]
fn golden_scope_definition_parses_validates_and_paths() {
    let scope = ScopeDefinition::from_yaml(GOLDEN_SCOPE).expect("valid golden YAML");
    scope.validate().expect("scope definition validates");

    assert_eq!(
        scope.resource_path().expect("resource path"),
        "projects/ovzdusie/policies/geo-sk-bb-radvan.yaml"
    );
}

#[test]
fn scope_definition_taxonomy_hierarchy_rules() {
    // /geo root with isRoot: true and no parent validates
    let root = ScopeDefinitionSpec {
        scope_string: "/geo".to_string(),
        is_root: true,
        is_child_of: None,
    };
    assert!(root.validate().is_ok());

    // /geo with isRoot: false fails
    let bad_root = ScopeDefinitionSpec {
        scope_string: "/geo".to_string(),
        is_root: false,
        is_child_of: None,
    };
    assert!(bad_root.validate().is_err());

    // /geo/SK/BB/Radvan with wrong parent fails
    let wrong_parent = ScopeDefinitionSpec {
        scope_string: "/geo/SK/BB/Radvan".to_string(),
        is_root: false,
        is_child_of: Some("/geo/SK".to_string()),
    };
    assert!(wrong_parent.validate().is_err());

    // Missing leading slash fails
    let no_slash = ScopeDefinitionSpec {
        scope_string: "geo/SK".to_string(),
        is_root: false,
        is_child_of: Some("geo".to_string()),
    };
    assert!(no_slash.validate().is_err());

    // Unknown root fails
    let unknown_root = ScopeDefinitionSpec {
        scope_string: "/weather/x".to_string(),
        is_root: false,
        is_child_of: Some("/weather".to_string()),
    };
    assert!(unknown_root.validate().is_err());

    // Trailing slash fails
    let trailing = ScopeDefinitionSpec {
        scope_string: "/geo/SK/".to_string(),
        is_root: false,
        is_child_of: Some("/geo".to_string()),
    };
    assert!(trailing.validate().is_err());

    // Double slash fails
    let double_slash = ScopeDefinitionSpec {
        scope_string: "/geo//SK".to_string(),
        is_root: false,
        is_child_of: Some("/geo".to_string()),
    };
    assert!(double_slash.validate().is_err());
}

/// GW4 and GW8: a policy says whether it grants or takes away, and the default is the
/// grant, so every manifest written before the field existed keeps its meaning.
#[test]
fn effect_defaults_to_permission_and_round_trips() {
    use jc_core::kinds::PolicyEffect;

    let granted = Policy::from_yaml(GOLDEN_POLICY).expect("the golden policy parses");
    assert_eq!(granted.spec.effect, PolicyEffect::Permission);
    assert!(!granted.spec.effect.is_prohibition());

    let denied = Policy::from_yaml(
        &GOLDEN_POLICY.replace("  assigner:", "  effect: prohibition\n  assigner:"),
    )
    .expect("a prohibition parses");
    denied.validate().expect("a prohibition is a valid policy");
    assert!(denied.spec.effect.is_prohibition());

    let round_tripped = Policy::from_yaml(&denied.to_yaml().expect("serializes"))
        .expect("a prohibition survives a round trip");
    assert_eq!(round_tripped.spec.effect, PolicyEffect::Prohibition);

    assert!(
        Policy::from_yaml(&GOLDEN_POLICY.replace("  assigner:", "  effect: forbid\n  assigner:"))
            .is_err(),
        "only the two documented effects parse"
    );
}

/// ADR-N-054, T-4666: a Policy may grant the platform actions alone; an action it does not know,
/// the same action twice, or a Policy granting nothing is refused, and a Policy without actions
/// renders as it did before them.
#[test]
fn a_policy_grants_platform_actions_beside_or_without_operations() {
    use jc_core::kinds::{PlatformAction, PolicySpec};
    let spec = |tail: &str| {
        serde_norway::from_str::<PolicySpec>(&format!(
            "contextSpaceRef: bike-trips\nassigner: did:web:banskabystrica.sk\n\
             assignee: {{ kind: role, id: steward }}\n{tail}"
        ))
    };
    let alone = spec("actions: [readVersions, personalData]\n").expect("parses");
    assert_eq!(
        alone.actions,
        [PlatformAction::ReadVersions, PlatformAction::PersonalData]
    );
    assert!(alone.operations.is_empty());
    alone.validate().expect("actions alone are a grant");

    assert!(spec("actions: [deleteEverything]\n").is_err());
    let twice = spec("actions: [readVersions, readVersions]\n").expect("parses");
    assert!(twice.validate().is_err());
    let nothing = spec("operations: []\n").expect("parses");
    assert!(nothing.validate().is_err());

    let plain = spec("operations: [queryEntity]\n").expect("parses");
    let rendered = serde_json::to_value(&plain).expect("renders");
    assert!(rendered.get("actions").is_none(), "{rendered}");
}

/// CIM 009 Table 4.20-2: `redirectionOps` writes and `federationOps` does not, the same answer
/// [`OperationGroup::includes_write`] gives (T-5035).
#[test]
fn has_write_reads_the_groups_as_the_table_does() {
    let mut p = Policy::from_yaml(GOLDEN_POLICY)
        .expect("valid golden YAML")
        .spec;
    p.operations = vec![OperationRef::Group(OperationGroup::RedirectionOps)];
    assert!(p.has_write());
    p.operations = vec![OperationRef::Group(OperationGroup::FederationOps)];
    assert!(!p.has_write());
}

/// AP-96: the roles an Endpoint hands out are its caller role and one level of named roles.
#[test]
fn is_endpoint_grant_takes_the_endpoints_own_roles_alone() {
    let mut p = Policy::from_yaml(GOLDEN_POLICY)
        .expect("valid golden YAML")
        .spec;
    let grant = |p: &mut jc_core::kinds::PolicySpec, kind, id: &str| {
        p.assignee = Principal::new(kind, id);
        p.is_endpoint_grant("helsinki", "app-alerts")
    };
    assert!(grant(
        &mut p,
        PrincipalKind::Role,
        "endpoint:helsinki/app-alerts"
    ));
    assert!(grant(
        &mut p,
        PrincipalKind::Role,
        "endpoint:helsinki/app-alerts/editor"
    ));
    assert!(!grant(
        &mut p,
        PrincipalKind::Role,
        "endpoint:helsinki/app-alerts/"
    ));
    assert!(!grant(
        &mut p,
        PrincipalKind::Role,
        "endpoint:helsinki/app-alerts/a/b"
    ));
    assert!(!grant(
        &mut p,
        PrincipalKind::Role,
        "endpoint:helsinki/app-alerts-2"
    ));
    assert!(!grant(&mut p, PrincipalKind::Role, "public"));
    assert!(!grant(
        &mut p,
        PrincipalKind::Group,
        "endpoint:helsinki/app-alerts"
    ));
}

const RULES_POLICY: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: Policy
metadata:
  name: kiosks
  namespace: bbsk
spec:
  contextSpaceRef: { kind: ContextSpace, name: kiosks }
  assigner: did:web:bbsk.sk
  rules:
    - assignee: { kind: role, id: public }
      operations: [retrieveOps]
    - effect: prohibition
      assignee: { kind: serviceAccount, id: kiosk-editor }
      operations: [updateAttrs]
      information:
        - entities: [{ type: Kiosk, id: "urn:ngsi-ld:Kiosk:bbsk.sk:kiosks:hlavne-namestie" }]
      q: "open==true"
  validity: { from: "2026-09-01T00:00:00Z" }
"#;

/// ADR-N-057 decision 1 (T-5036): the rules shape parses, validates, round-trips as rules, and
/// expands to one single-rule spec per rule named `{policy}#{n}` sharing assigner and validity.
#[test]
fn rules_policy_roundtrips_and_expands_one_grant_per_rule() {
    let p = Policy::from_yaml(RULES_POLICY).expect("the rules shape parses");
    p.validate().expect("the rules shape validates");
    let yaml = p.to_yaml().expect("serialize");
    assert!(
        !yaml.contains("assignee: {kind") && yaml.contains("rules:"),
        "{yaml}"
    );
    assert!(!yaml.contains("operations: []"), "{yaml}");
    assert_eq!(p, Policy::from_yaml(&yaml).expect("re-import"));

    let rules = p.spec.expand("kiosks").expect("a valid shape expands");
    let names: Vec<&str> = rules.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(names, ["kiosks#1", "kiosks#2"]);
    let (_, editor) = &rules[1];
    assert!(editor.effect.is_prohibition());
    assert_eq!(
        editor.assignee,
        Principal::new(PrincipalKind::ServiceAccount, "kiosk-editor")
    );
    assert_eq!(editor.q.as_deref(), Some("open==true"));
    assert_eq!(editor.assigner, "did:web:bbsk.sk");
    assert!(editor.validity.is_some() && editor.rules.is_none());
    assert_eq!(jc_core::kinds::PolicySpec::policy_of("kiosks#2"), "kiosks");
    assert_eq!(jc_core::kinds::PolicySpec::policy_of("kiosks"), "kiosks");
}

/// The single-rule shape stays valid and is a list of one under its own name.
#[test]
fn single_rule_policy_expands_to_itself() {
    let p = Policy::from_yaml(GOLDEN_POLICY).expect("valid golden YAML");
    let rules = p.spec.expand("public-air-quality").expect("expands");
    assert_eq!(
        rules,
        vec![("public-air-quality".to_owned(), p.spec.clone())]
    );
}

/// Both shapes at once, an empty `rules[]`, or neither shape is refused, never half read.
#[test]
fn both_shapes_empty_rules_or_no_rule_are_refused() {
    let both = RULES_POLICY.replace("  rules:\n", "  operations: [deleteEntity]\n  rules:\n");
    let both = Policy::from_yaml(&both).expect("parses");
    let error = both.validate().expect_err("both shapes are refused");
    assert!(error.to_string().contains("operations"), "{error}");
    assert!(both.spec.expand("kiosks").is_err());

    let empty = r#"apiVersion: joinedcontext.com/v1alpha1
kind: Policy
metadata: { name: kiosks, namespace: bbsk }
spec:
  contextSpaceRef: kiosks
  assigner: did:web:bbsk.sk
  rules: []
"#;
    let empty = Policy::from_yaml(empty).expect("parses");
    assert!(empty.validate().is_err(), "an empty rules[] is refused");
    assert!(empty.spec.expand("kiosks").is_err());

    let neither = Policy::from_yaml(&RULES_POLICY[..RULES_POLICY.find("  rules:").expect("rules")])
        .expect("parses");
    assert!(neither.validate().is_err(), "no assignee is refused");
    assert!(
        neither.spec.expand("kiosks").is_err(),
        "a Policy with neither shape expands to no grant"
    );

    let unknown = RULES_POLICY.replace("q: \"open==true\"", "validity: {}");
    assert!(
        Policy::from_yaml(&unknown).is_err(),
        "validity stays on the Policy"
    );
}

/// PF-126, ADR-N-061: a `user` assignee is a Keycloak user id or a client's own
/// `service-account-<client>` user; an e-mail or a username is refused with the field named.
#[test]
fn a_user_assignee_is_a_keycloak_id_never_an_e_mail() {
    let mut p = Policy::from_yaml(GOLDEN_POLICY)
        .expect("valid golden YAML")
        .spec;
    for id in [
        "3f2a9c1e-7b4d-4e8a-9c61-0d5e2b7f8a14",
        "service-account-portal-reconciler",
    ] {
        p.assignee = Principal::new(PrincipalKind::User, id);
        p.validate().unwrap_or_else(|err| panic!("{id}: {err}"));
    }
    for id in [
        "demo.steward@hel.fi",
        "demo.steward",
        "service-account-",
        "service-account-a b",
        "3F2A9C1E-7B4D-4E8A-9C61-0D5E2B7F8A14",
    ] {
        p.assignee = Principal::new(PrincipalKind::User, id);
        let err = p.validate().expect_err(id).to_string();
        assert!(err.contains("spec.assignee.id"), "{id}: {err}");
        assert!(err.contains("Keycloak user id"), "{id}: {err}");
    }
    // The other kinds keep their own names.
    p.assignee = Principal::new(PrincipalKind::Group, "air-quality-team");
    p.validate().expect("a group assignee is a name");
}
