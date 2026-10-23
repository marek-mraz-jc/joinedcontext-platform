//! A Policy rendered as an ODRL `Set` in the `ngsi-ld:` profile imports back to the same manifest,
//! and every ODRL member the profile does not map is refused by name (ADR-N-057 decision 4,
//! T-5039).
//!
//! `writes_the_records_the_shapes_check` writes the rendered Sets and the refused documents to
//! `ODRL_RECORDS_DIR` (else `target/tmp/odrl-records`); `scripts/ci/validate-odrl-profile.py`
//! checks the SHACL shapes in `schemas/odrl/ngsi-ld-v1` accept the first and refuse the second,
//! so the shapes and this reader refuse the same documents.

use jc_core::kinds::policy_odrl::{import, render, Names, CONTEXTS};
use jc_core::kinds::Policy;
use serde_json::{json, Value};

/// Policies of the shapes the seeds and the Portal write: an App's history grant, a public grant
/// with every residual filter, a service account over several selectors, and `rules[]`.
const POLICIES: &[&str] = &[
    include_str!("golden/006-Policy-03-domain-model.yaml"),
    r#"
apiVersion: joinedcontext.com/v1alpha1
kind: Policy
metadata: { name: app-bike-weather-demand-2-history, namespace: helsinki }
spec:
  assignee: { id: "endpoint:helsinki/app-bike-weather-demand", kind: role }
  assigner: did:web:hel.fi
  contextSpaceRef: { kind: ContextSpace, name: helsinki }
  information:
  - entities: [{ type: WeatherObserved }]
    propertyNames: [name, location, temperature]
    relationshipNames: [name, location, temperature]
  operations: [queryTemporal, retrieveTemporal]
  temporalQ: timerel=after;timeAt=P-7D
"#,
    r#"
apiVersion: joinedcontext.com/v1alpha1
kind: Policy
metadata: { name: sensors-writer, namespace: zilina }
spec:
  assignee: { id: pl-sensors, kind: serviceAccount }
  assigner: did:web:zilina.sk
  contextSpaceRef: { kind: ContextSpace, name: mesto }
  information:
  - entities:
    - { type: Device, id: "urn:ngsi-ld:Device:zilina:1" }
    - { type: Device, id: "urn:ngsi-ld:Device:zilina:2" }
    propertyNames: [battery]
  - entities: [{ type: DeviceModel }]
  - entities: [{ type: Building }]
  operations: [updateOps, createEntity]
"#,
    r#"
apiVersion: joinedcontext.com/v1alpha1
kind: Policy
metadata: { name: kiosks, namespace: praha }
spec:
  assigner: did:web:praha.eu
  contextSpaceRef: { kind: ContextSpace, name: mesto }
  validity: { from: "2026-09-01T00:00:00Z", to: "2027-09-01T00:00:00Z" }
  rules:
  - assignee: { kind: role, id: public }
    operations: [retrieveOps]
    information: [{ entities: [{ type: Kiosk }] }]
  - assignee: { kind: did, id: "did:web:partner.example" }
    operations: [updateAttrs]
    actions: [readVersions]
    information: [{ entities: [{ type: Kiosk, id: "urn:ngsi-ld:Kiosk:1" }], relationshipNames: [refStop] }]
    geoQ: "georel=within;geometry=Polygon;coordinates=[[[14.4,50.0],[14.5,50.0],[14.5,50.1],[14.4,50.0]]]"
  - effect: prohibition
    assignee: { kind: group, id: contractors }
    operations: [deleteEntity]
"#,
];

fn policies() -> Vec<Policy> {
    POLICIES
        .iter()
        .map(|yaml| Policy::from_yaml(yaml).expect("fixture parses"))
        .collect()
}

/// The manifest an import writes: the grant without the file's own title and annotations.
fn grant_of(policy: &Policy) -> Policy {
    let mut policy = policy.clone();
    policy.metadata.title = None;
    policy.metadata.annotations.clear();
    policy
}

#[test]
fn every_policy_imports_back_to_the_same_manifest() {
    for policy in policies() {
        policy.validate().expect("fixture is valid");
        let set = render(&policy).expect("renders");
        assert_eq!(set["@context"], json!(CONTEXTS));
        assert_eq!(set["conflict"], "prohibit");
        let back = import(&set, &Names::default()).expect("imports");
        assert_eq!(back, grant_of(&policy), "{set:#}");
    }
}

#[test]
fn a_rule_per_selector_like_the_broker() {
    let set = render(&policies()[2]).expect("renders");
    let rules = set["permission"].as_array().expect("permission");
    assert_eq!(rules.len(), 4);
    assert_eq!(
        rules[0]["action"],
        json!(["ngsi-ld:updateOps", "ngsi-ld:createEntity"])
    );
    assert_eq!(
        rules[0]["assignee"],
        json!({"@type": "ngsi-ld:ServiceAccount", "uid": "pl-sensors"})
    );
    assert_eq!(
        rules[1]["target"],
        json!({"@type": "ngsi-ld:EntityType", "uid": "Device", "refinement": [
            {"leftOperand": "ngsi-ld:id", "operator": "eq", "rightOperand": "urn:ngsi-ld:Device:zilina:2"},
            {"leftOperand": "ngsi-ld:propertyNames", "operator": "isAnyOf", "rightOperand": ["battery"]}],
            "partOf": "#information-1"})
    );
    assert_eq!(rules[3]["target"]["partOf"], "#information-3");
    assert_eq!(
        set["uid"],
        "urn:joinedcontext:policy:zilina:mesto:sensors-writer"
    );
}

/// The broker's own access document (API/02 §7b): a DID assignee on the Set, `attrs`, and
/// `geoQ` and `temporalQ` decomposed into a geometry and `dateTime` bounds.
fn broker_set() -> Value {
    json!({
        "@context": CONTEXTS, "@type": "Set",
        "uid": "https://hel.example.fi/api/endpoint/zt4q/access#sha256:4a1f",
        "assigner": "did:web:hel.fi", "assignee": "did:web:partner.example",
        "permission": [{
            "action": ["ngsi-ld:queryEntity", "ngsi-ld:retrieveEntity"],
            "target": {"@type": "ngsi-ld:EntityType", "uid": "AirQualityObserved",
                       "refinement": [{"leftOperand": "ngsi-ld:attrs", "operator": "isAnyOf", "rightOperand": ["pm10"]}]},
            "assigner": "did:web:hel.fi",
            "constraint": [
                {"leftOperand": "ngsi-ld:scopeQ", "operator": "isPartOf", "rightOperand": "/geo/FI/HKI"},
                {"leftOperand": "ngsi-ld:geoQ", "operator": "ngsi-ld:within",
                 "rightOperand": {"@type": "geojson:Polygon", "coordinates": [[[0,0],[1,0],[1,1],[0,0]]]}},
                {"leftOperand": "dateTime", "operator": "gteq", "rightOperand": "2026-01-01T00:00:00Z"},
                {"leftOperand": "dateTime", "operator": "lteq", "rightOperand": "2026-02-01T00:00:00Z"}
            ]
        }]
    })
}

fn names() -> Names {
    Names {
        project: Some("helsinki".into()),
        space: Some("ilma".into()),
        name: Some("partner-air".into()),
    }
}

#[test]
fn the_brokers_document_imports_with_the_names_given() {
    let policy = import(&broker_set(), &names()).expect("imports");
    assert_eq!(policy.metadata.name, "partner-air");
    assert_eq!(policy.metadata.namespace.as_deref(), Some("helsinki"));
    let spec = &policy.spec;
    assert_eq!(spec.context_space_ref.name(), "ilma");
    assert_eq!(spec.assignee.id, "did:web:partner.example");
    assert_eq!(spec.information[0].property_names, ["pm10"]);
    assert_eq!(spec.information[0].relationship_names, ["pm10"]);
    assert_eq!(
        spec.geo_q.as_deref(),
        Some("georel=within;geometry=Polygon;coordinates=[[[0,0],[1,0],[1,1],[0,0]]]")
    );
    assert_eq!(
        spec.temporal_q.as_deref(),
        Some("timerel=between;timeAt=2026-01-01T00:00:00Z;endTimeAt=2026-02-01T00:00:00Z")
    );
    let missing = import(&broker_set(), &Names::default()).expect_err("no names");
    assert_eq!(missing.member, "uid");
}

#[test]
fn given_names_win_over_the_uid() {
    let set = render(&policies()[1]).expect("renders");
    let policy = import(&set, &names()).expect("imports");
    assert_eq!(policy.metadata.name, "partner-air");
    assert_eq!(policy.spec.context_space_ref.name(), "ilma");
}

/// A change to the broker's Set and the member its refusal names; `shacl` when the shapes see
/// the member too (a JSON-LD processor drops an unknown key before SHACL reads the graph).
fn refusals() -> Vec<(&'static str, Value, &'static str, bool)> {
    let base = broker_set();
    let with = |pointer: &str, value: Value| {
        let mut doc = base.clone();
        *doc.pointer_mut(pointer).expect("pointer") = value;
        doc
    };
    let rule = |member: &str, value: Value| {
        let mut doc = base.clone();
        doc["permission"][0][member] = value;
        doc
    };
    let set = |member: &str, value: Value| {
        let mut doc = base.clone();
        doc[member] = value;
        doc
    };
    let constraint = |left: &str, op: &str, right: Value| {
        with(
            "/permission/0/constraint/0",
            json!({"leftOperand": left, "operator": op, "rightOperand": right}),
        )
    };
    vec![
        (
            "duty",
            rule("duty", json!([{"action": "attribute"}])),
            "permission[0].duty",
            true,
        ),
        (
            "obligation",
            set(
                "obligation",
                json!([{"action": "compensate", "assigner": "did:web:hel.fi"}]),
            ),
            "obligation",
            true,
        ),
        (
            "conflict-perm",
            set("conflict", json!("perm")),
            "conflict",
            true,
        ),
        ("offer", set("@type", json!("Offer")), "@type", true),
        ("agreement", set("@type", json!("Agreement")), "@type", true),
        (
            "left-operand",
            constraint("spatial", "eq", json!("SK")),
            "permission[0].constraint[0].leftOperand",
            true,
        ),
        (
            "action",
            with("/permission/0/action/0", json!("use")),
            "permission[0].action[0]",
            true,
        ),
        (
            "unknown-action",
            with("/permission/0/action/0", json!("ngsi-ld:readEverything")),
            "permission[0].action[0]",
            false,
        ),
        (
            "operator",
            constraint("ngsi-ld:q", "neq", json!("a==1")),
            "permission[0].constraint[0].operator",
            true,
        ),
        (
            "near",
            constraint(
                "ngsi-ld:geoQ",
                "ngsi-ld:near",
                json!({"@type": "geojson:Point", "coordinates": [0, 0]}),
            ),
            "permission[0].constraint[0].operator",
            true,
        ),
        (
            "refinement",
            with(
                "/permission/0/target/refinement/0/leftOperand",
                json!("ngsi-ld:attrsLike"),
            ),
            "permission[0].target.refinement[0].leftOperand",
            true,
        ),
        (
            "context",
            set("@context", json!(["http://www.w3.org/ns/odrl.jsonld"])),
            "@context",
            false,
        ),
        (
            "unknown-member",
            rule("note", json!("x")),
            "permission[0].note",
            false,
        ),
        (
            "plain-assignee",
            set("assignee", json!("public")),
            "assignee",
            false,
        ),
        (
            "other-assigner",
            rule("assigner", json!("did:web:other.example")),
            "permission[0].assigner",
            false,
        ),
        ("nothing", set("permission", json!([])), "permission", false),
    ]
}

#[test]
fn each_unmapped_member_is_refused_by_name() {
    for (case, doc, member, _) in refusals() {
        let refusal = import(&doc, &names()).expect_err(case);
        assert_eq!(refusal.member, member, "{case}: {refusal}");
    }
}

#[test]
fn validity_belongs_to_the_whole_policy() {
    let mut set = render(&policies()[3]).expect("renders");
    let first = set["permission"][0]["constraint"]
        .as_array_mut()
        .expect("constraints");
    first.pop();
    let refusal = import(&set, &Names::default()).expect_err("validity differs");
    assert_eq!(refusal.member, "constraint");
}

#[test]
fn an_invalid_manifest_is_refused_not_repaired() {
    let doc = {
        let mut doc = broker_set();
        doc["permission"][0]["target"]["refinement"] = json!([{"leftOperand": "ngsi-ld:idPattern", "operator": "eq", "rightOperand": "urn:a:.*"}]);
        doc
    };
    let refusal = import(&doc, &names()).expect_err("unanchored idPattern");
    assert_eq!(refusal.member, "(manifest)");
    assert!(refusal.reason.contains("anchored"), "{refusal}");
}

#[test]
fn writes_the_records_the_shapes_check() {
    let directory = std::env::var("ODRL_RECORDS_DIR")
        .unwrap_or_else(|_| format!("{}/odrl-records", env!("CARGO_TARGET_TMPDIR")));
    for sub in ["accepted", "refused"] {
        std::fs::create_dir_all(format!("{directory}/{sub}")).expect("records directory");
    }
    let write = |path: String, doc: &Value| {
        std::fs::write(path, serde_json::to_string_pretty(doc).expect("json")).expect("written")
    };
    for (n, policy) in policies().iter().enumerate() {
        write(
            format!("{directory}/accepted/policy-{n}.jsonld"),
            &render(policy).expect("renders"),
        );
    }
    write(format!("{directory}/accepted/broker.jsonld"), &broker_set());
    for (case, doc, _, shacl) in refusals() {
        if shacl {
            write(format!("{directory}/refused/{case}.jsonld"), &doc);
        }
    }
}
