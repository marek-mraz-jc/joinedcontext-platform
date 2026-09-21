//! T-0303: what `jcctl apply` does to a broker's registrations (MF-36, SP-09, CC-18). A city
//! joins or leaves a federation through these manifests alone (CC-14): `apply` registers,
//! `remove` withdraws, and no broker-specific step sits beside either.
//!
//! The reconciler's whole job is the third run: after create and update, an apply over an
//! unchanged repository has to make no writing call at all. Everything else here holds that
//! property up against the things that would quietly break it — a member the broker adds by
//! itself, a `PATCH` body carrying an immutable member, a registration whose name changed.

use jcctl::csr::{
    apply, forwards_caller_identity, registration, registration_id, remove, CsrError, Endpoints,
    InMemoryBroker, MemberSource, Outcome,
};
use jcctl::loader::RawManifest;
use serde_json::json;

/// The tenant of the space the registration belongs to. It travels on the request, never in
/// the body (SP-08, SP-09).
const TENANT: &str = "bb-ovzdusie";

const LOCAL: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: ContextSourceRegistration
metadata:
  name: ovzdusie-mesto
  namespace: bb-hub
  title: { sk: "Ovzdušie mesta", en: "City air quality" }
spec:
  contextSpaceRef: hub
  endpointRef: { kind: Endpoint, name: ovzdusie-read }
  information:
    - entities:
        - type: AirQualityObserved
          idPattern: "^urn:ngsi-ld:AirQualityObserved:banskabystrica:.*$"
      propertyNames: [pm10, pm25]
  federation:
    identity: serviceAccount
    serviceAccountRef: { kind: ServiceAccount, name: hub-reader }
  mode: exclusive
"#;

const EXTERNAL: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: ContextSourceRegistration
metadata:
  name: zvolen-doprava
  namespace: bb-hub
spec:
  contextSpaceRef: hub
  endpoint: https://context.zvolen.sk/ngsi-ld/v1
  information:
    - entities: [{ type: Vehicle }]
  federation:
    identity: caller
  operations: [federationOps]
  expiresAt: 2027-01-01T00:00:00Z
"#;

fn manifest(yaml: &str) -> RawManifest {
    serde_norway::from_str(yaml).expect("the manifest parses")
}

/// The table the reconciler holds: an Endpoint name to the broker that holds its space and the
/// tenant that space is (PF-48 direct read).
fn endpoints() -> impl Endpoints {
    |name: &str| match name {
        "ovzdusie-read" => Some(MemberSource {
            endpoint: "http://antares.brokers.svc.cluster.local/ngsi-ld/v1".into(),
            tenant: "bb-ovzdusie".into(),
        }),
        _ => None,
    }
}

/// A source that is nowhere resolves to nothing, which is the point of `Endpoints`.
fn no_endpoints() -> impl Endpoints {
    |_: &str| None
}

#[test]
fn a_local_endpoint_becomes_a_registration_at_that_endpoints_address() {
    let body = registration(&manifest(LOCAL), &endpoints()).expect("builds");

    assert_eq!(
        body["id"],
        json!("urn:ngsi-ld:ContextSourceRegistration:ovzdusie-mesto")
    );
    assert_eq!(body["type"], json!("ContextSourceRegistration"));
    assert_eq!(
        body["endpoint"],
        json!("http://antares.brokers.svc.cluster.local/ngsi-ld/v1")
    );
    // The member is read as its own tenant of that broker, which is the whole of the direct
    // read: no credential travels, and the hub Endpoint's policy set is the only gate (PF-48).
    assert_eq!(body["tenant"], json!("bb-ovzdusie"));
    assert_eq!(body["mode"], json!("exclusive"));
    assert_eq!(
        body["information"],
        json!([{
            "entities": [{
                "type": "AirQualityObserved",
                "idPattern": "^urn:ngsi-ld:AirQualityObserved:banskabystrica:.*$"
            }],
            "propertyNames": ["pm10", "pm25"]
        }])
    );
}

#[test]
fn an_external_source_keeps_its_own_url_and_its_expiry() {
    let body = registration(&manifest(EXTERNAL), &no_endpoints()).expect("builds");

    assert_eq!(
        body["endpoint"],
        json!("https://context.zvolen.sk/ngsi-ld/v1")
    );
    assert_eq!(body["expiresAt"], json!("2027-01-01T00:00:00Z"));
    // CIM 009 clause 5.2.9: which operations the source answers for. Passed through as the
    // manifest wrote it, because the vocabulary is the broker's and not ours.
    assert_eq!(body["operations"], json!(["federationOps"]));
    // A registration that names no operation carries none, so the broker applies its own
    // default rather than being told a list we invented.
    assert!(registration(&manifest(LOCAL), &endpoints())
        .expect("builds")
        .get("operations")
        .is_none());
    // The default when the manifest is silent, spelled out rather than left off: a broker that
    // defaults differently would federate a different meaning than the file says.
    assert_eq!(body["mode"], json!("inclusive"));
}

/// A registration this platform cannot address is a plan that would create a broken source, so
/// it fails before any call rather than registering an empty address.
#[test]
fn an_endpoint_this_repository_does_not_know_is_refused_by_name() {
    assert_eq!(
        registration(&manifest(LOCAL), &no_endpoints()),
        Err(CsrError::UnresolvedEndpoint("ovzdusie-read".into()))
    );
}

/// CC-18: create, then converge. The second and third runs are the ones that matter.
#[test]
fn a_second_apply_over_an_unchanged_manifest_makes_no_writing_call() {
    let mut broker = InMemoryBroker::new();
    let manifest = manifest(LOCAL);

    assert_eq!(
        apply(&mut broker, TENANT, &manifest, &endpoints()),
        Ok(Outcome::Created)
    );
    assert_eq!(
        apply(&mut broker, TENANT, &manifest, &endpoints()),
        Ok(Outcome::Unchanged)
    );
    assert_eq!(
        apply(&mut broker, TENANT, &manifest, &endpoints()),
        Ok(Outcome::Unchanged)
    );

    assert_eq!(broker.calls().len(), 1, "writes: {:?}", broker.calls());
    assert_eq!(broker.calls()[0].0, "create");
}

#[test]
fn a_changed_claim_is_patched_without_the_members_the_broker_owns() {
    let mut broker = InMemoryBroker::new();
    apply(&mut broker, TENANT, &manifest(LOCAL), &endpoints()).expect("created");

    let widened = manifest(&LOCAL.replace("[pm10, pm25]", "[pm10, pm25, no2]"));
    assert_eq!(
        apply(&mut broker, TENANT, &widened, &endpoints()),
        Ok(Outcome::Updated)
    );

    let id = registration_id("ovzdusie-mesto");
    let stored = broker.registration(TENANT, &id).expect("still there");
    assert_eq!(
        stored["information"][0]["propertyNames"],
        json!(["pm10", "pm25", "no2"])
    );
    // The id is in the path. A body repeating it is a change to an immutable member, which the
    // specification has the broker refuse.
    assert!(!broker
        .last_body()
        .expect("a patch")
        .as_object()
        .unwrap()
        .contains_key("id"));

    // And the widened manifest is now what "unchanged" means.
    assert_eq!(
        apply(&mut broker, TENANT, &widened, &endpoints()),
        Ok(Outcome::Unchanged)
    );
    assert_eq!(broker.calls().len(), 2, "writes: {:?}", broker.calls());
}

/// CC-69: the broker stamps its own members onto a registration. Reading those back as drift
/// would rewrite every registration on every apply, which is the churn this reconciler exists
/// to avoid.
#[test]
fn the_members_the_broker_adds_by_itself_are_not_drift() {
    let mut broker = InMemoryBroker::new();
    let manifest = manifest(LOCAL);
    apply(&mut broker, TENANT, &manifest, &endpoints()).expect("created");

    broker.stamp(
        TENANT,
        &registration_id("ovzdusie-mesto"),
        [
            ("createdAt", json!("2026-09-06T08:00:00Z")),
            ("modifiedAt", json!("2026-09-06T08:00:00Z")),
            (
                "@context",
                json!(["https://uri.etsi.org/ngsi-ld/v1/ngsi-ld-core-context.jsonld"]),
            ),
            ("status", json!("ok")),
        ],
    );

    assert_eq!(
        apply(&mut broker, TENANT, &manifest, &endpoints()),
        Ok(Outcome::Unchanged)
    );
    assert_eq!(broker.calls().len(), 1, "writes: {:?}", broker.calls());
}

/// SP-08, SP-09: the tenant is the space's, it is carried by the request, and it is in no body.
#[test]
fn the_registration_is_written_into_the_spaces_tenant_and_no_other() {
    let mut broker = InMemoryBroker::new();
    apply(&mut broker, TENANT, &manifest(LOCAL), &endpoints()).expect("created");
    apply(
        &mut broker,
        "zvolen-doprava",
        &manifest(EXTERNAL),
        &no_endpoints(),
    )
    .expect("created");

    assert_eq!(broker.tenants(), vec![TENANT, "zvolen-doprava"]);
    for (operation, tenant, _) in broker.calls() {
        assert!(
            !tenant.is_empty(),
            "{operation} was made without a tenant, which the broker would read as the default"
        );
    }

    // Two tenants, two registrations, and neither knows about the other's.
    let id = registration_id("ovzdusie-mesto");
    assert!(broker.registration(TENANT, &id).is_some());
    assert!(broker.registration("zvolen-doprava", &id).is_none());

    let body = registration(&manifest(LOCAL), &endpoints()).expect("builds");
    let text = serde_json::to_string(&body).expect("serialises");
    // `tenant` is the member's own, a CIM 009 member of the registration (clause 5.2.9), and
    // the only thing the direct read needs. What must never appear is a header of the internal
    // hop or a credential.
    for member in ["NGSILD-Tenant", "Authorization"] {
        assert!(
            !text.contains(member),
            "the payload carries {member}:\n{text}"
        );
    }
}

/// PF-48: which identity a forward carries is a decision the gateway reads at request time.
/// Nothing about it belongs in a registration the broker stores.
#[test]
fn no_identity_and_no_credential_reaches_the_broker() {
    for yaml in [LOCAL, EXTERNAL] {
        let manifest = manifest(yaml);
        let body = registration(
            &manifest,
            &(|name: &str| {
                Some(MemberSource {
                    endpoint: format!("https://elsewhere.example/{name}"),
                    tenant: "a-member-space".into(),
                })
            }),
        )
        .expect("builds");
        // By member, not by substring: `operations: [federationOps]` is a legitimate CIM 009
        // value that contains the word `federation`, and a text search would call it a leak.
        let members: Vec<&str> = body
            .as_object()
            .expect("an object")
            .keys()
            .map(String::as_str)
            .collect();
        for forbidden in ["federation", "identity", "serviceAccountRef"] {
            assert!(
                !members.contains(&forbidden),
                "the payload carries {forbidden}: {members:?}"
            );
        }
        let text = serde_json::to_string(&body).expect("serialises");
        assert!(
            !text.contains("hub-reader"),
            "the account this platform forwards as reached the broker:\n{text}"
        );
    }

    let spec = |yaml| {
        serde_json::from_value::<jc_core::kinds::ContextSourceRegistrationSpec>(manifest(yaml).spec)
            .expect("the spec parses")
    };
    assert!(forwards_caller_identity(&spec(EXTERNAL)));
    assert!(!forwards_caller_identity(&spec(LOCAL)));
}

/// CC-14: leaving a federation is the registration manifest leaving the repository; the next
/// apply withdraws it from the broker once and a repeat finds nothing left to do.
#[test]
fn a_registration_the_repository_dropped_is_deleted_once() {
    let mut broker = InMemoryBroker::new();
    apply(&mut broker, TENANT, &manifest(LOCAL), &endpoints()).expect("created");

    assert_eq!(
        remove(&mut broker, TENANT, "ovzdusie-mesto"),
        Ok(Outcome::Deleted)
    );
    assert!(broker
        .registration(TENANT, &registration_id("ovzdusie-mesto"))
        .is_none());

    // An apply has to be re-runnable after a partial failure, so removing what is already gone
    // is a no-op and not an error the next run trips over.
    assert_eq!(
        remove(&mut broker, TENANT, "ovzdusie-mesto"),
        Ok(Outcome::Unchanged)
    );
    assert_eq!(broker.calls().len(), 2, "writes: {:?}", broker.calls());
}

#[test]
fn a_manifest_of_another_kind_is_not_a_registration() {
    let other = manifest(
        "apiVersion: joinedcontext.com/v1alpha1\nkind: Endpoint\nmetadata: { name: x }\nspec: {}\n",
    );
    assert_eq!(
        registration(&other, &endpoints()),
        Err(CsrError::NotARegistration)
    );
}

/// The manifest's own validation is the reconciler's: a claim of nothing never reaches a
/// broker that would accept it and federate nothing.
#[test]
fn a_registration_that_claims_nothing_never_reaches_the_broker() {
    let mut broker = InMemoryBroker::new();
    let empty = manifest(
        "apiVersion: joinedcontext.com/v1alpha1\nkind: ContextSourceRegistration\n\
         metadata: { name: nic }\nspec:\n  contextSpaceRef: hub\n  \
         endpoint: https://elsewhere.example/ngsi-ld/v1\n  information: []\n  \
         federation: { identity: caller }\n",
    );

    assert!(matches!(
        apply(&mut broker, TENANT, &empty, &no_endpoints()),
        Err(CsrError::Spec(_))
    ));
    assert!(broker.calls().is_empty(), "writes: {:?}", broker.calls());
}

// --- T-2528: the edge cases of `information`, the claim a registration makes (MF-36, SP-09) ---

/// The `information` the broker is told for `EXTERNAL` with its claim replaced by `claim`.
fn told(claim: &str) -> serde_json::Value {
    let yaml = EXTERNAL.replace(
        "  information:\n    - entities: [{ type: Vehicle }]\n",
        &format!("  information:\n{claim}"),
    );
    registration(&manifest(&yaml), &no_endpoints()).expect("builds")["information"].clone()
}

/// SP-09: the broker is told exactly what the manifest claims, and nothing the spec did not set.
#[test]
fn the_entities_array_never_carries_a_field_the_spec_did_not_set() {
    assert_eq!(
        told("    - entities: [{ type: Vehicle }]\n"),
        json!([{ "entities": [{ "type": "Vehicle" }] }])
    );
}

/// SP-09: a claim of one exact entity names it by id, as a string.
#[test]
fn an_exact_entity_id_selector_is_carried_as_a_string() {
    let id = "urn:ngsi-ld:Vehicle:zvolen.sk:doprava:bus-12";
    assert_eq!(
        told(&format!(
            "    - entities: [{{ type: Vehicle, id: \"{id}\" }}]\n"
        )),
        json!([{ "entities": [{ "type": "Vehicle", "id": id }] }])
    );
}

/// SP-09: relationships the source serves are claimed like its properties.
#[test]
fn relationship_names_are_carried_when_declared() {
    assert_eq!(
        told("    - entities: [{ type: Vehicle }]\n      relationshipNames: [refRoute, refStop]\n"),
        json!([{ "entities": [{ "type": "Vehicle" }], "relationshipNames": ["refRoute", "refStop"] }])
    );
}

/// SP-09: an id and a pattern together are both carried; the broker narrows by both.
#[test]
fn an_entity_selector_with_both_id_and_idpattern_carries_both() {
    let id = "urn:ngsi-ld:Vehicle:zvolen.sk:doprava:bus-12";
    let body = told(&format!(
        "    - entities: [{{ type: Vehicle, id: \"{id}\", idPattern: \"^urn:ngsi-ld:Vehicle:zvolen.sk:.*$\" }}]\n"
    ));
    assert_eq!(body[0]["entities"][0]["id"], json!(id));
    assert_eq!(
        body[0]["entities"][0]["idPattern"],
        json!("^urn:ngsi-ld:Vehicle:zvolen.sk:.*$")
    );
}

/// SP-09: an entry that names no attribute omits both lists rather than claiming empty ones.
#[test]
fn an_information_entry_with_no_property_or_relationship_names_omits_both_keys() {
    let body = told("    - entities: [{ type: Vehicle }]\n      propertyNames: []\n      relationshipNames: []\n");
    let entry = body[0].as_object().expect("an entry");
    assert!(!entry.contains_key("propertyNames"), "{body}");
    assert!(!entry.contains_key("relationshipNames"), "{body}");
}

/// SP-09: several entries are carried in the order the manifest declares them.
#[test]
fn multiple_information_entries_are_carried_in_declared_order() {
    let body = told(
        "    - entities: [{ type: Vehicle }]\n    - entities: [{ type: BusStop }]\n    - entities: [{ type: Route }]\n",
    );
    let types: Vec<_> = body
        .as_array()
        .expect("a list")
        .iter()
        .map(|entry| entry["entities"][0]["type"].clone())
        .collect();
    assert_eq!(
        types,
        vec![json!("Vehicle"), json!("BusStop"), json!("Route")]
    );
}

/// SP-09: every selector of one entry is carried.
#[test]
fn multiple_entity_selectors_in_one_entry_are_all_carried() {
    assert_eq!(
        told("    - entities: [{ type: Vehicle }, { type: BusStop }]\n      propertyNames: [location]\n"),
        json!([{ "entities": [{ "type": "Vehicle" }, { "type": "BusStop" }], "propertyNames": ["location"] }])
    );
}

/// SP-09: a name with a dot or an at sign is the attribute's own name and is not rewritten.
#[test]
fn a_property_name_containing_a_dot_or_at_sign_is_carried_unescaped() {
    assert_eq!(
        told("    - entities: [{ type: Vehicle }]\n      propertyNames: [\"speed.value\", \"@context\"]\n")[0]
            ["propertyNames"],
        json!(["speed.value", "@context"])
    );
}

/// MF-36: an empty claim never reaches the broker; the existing guard names why.
#[test]
fn an_empty_information_list_never_becomes_a_registration() {
    let yaml = EXTERNAL.replace(
        "  information:\n    - entities: [{ type: Vehicle }]\n",
        "  information: []\n",
    );
    assert!(registration(&manifest(&yaml), &no_endpoints()).is_err());
}

/// MF-36: an entry that selects no entity claims nothing and is refused like an empty list.
#[test]
fn an_information_entry_with_an_empty_entities_list_is_refused() {
    let yaml = EXTERNAL.replace(
        "  information:\n    - entities: [{ type: Vehicle }]\n",
        "  information:\n    - entities: []\n      propertyNames: [speed]\n",
    );
    assert!(registration(&manifest(&yaml), &no_endpoints()).is_err());
}
