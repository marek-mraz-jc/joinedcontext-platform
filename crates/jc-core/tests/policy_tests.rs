use jc_core::kinds::policy::{Operation, OperationRef, ScopeDefinitionSpec, Validity};
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

    // residual_is_empty on golden is false; when cleared is true
    assert!(!p.spec.residual_is_empty());
    let mut empty_residual = p.spec.clone();
    empty_residual.q = None;
    empty_residual.scope_q = None;
    empty_residual.geo_q = None;
    empty_residual.temporal_q = None;
    assert!(empty_residual.residual_is_empty());

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
