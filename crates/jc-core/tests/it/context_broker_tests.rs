//! `kind: ContextBroker` and `ContextSpace.spec.brokerRef` (PF-110, PF-111, ADR-N-047).
use jc_core::envelope::Kind;
use jc_core::kinds::{broker_of, validate_brokers, ContextBrokerSpec, IMPLICIT_BROKER};
use jc_core::registry;

const DIGEST: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn broker(default: bool) -> ContextBrokerSpec {
    serde_json::from_value(serde_json::json!({
        "default": default,
        "image": format!("ghcr.io/joinedcontext/antares@sha256:{DIGEST}"),
    }))
    .expect("a broker")
}

#[test]
fn a_broker_manifest_validates_and_lives_beside_the_organization() {
    let yaml = format!(
        "apiVersion: joinedcontext.com/v1alpha1\nkind: ContextBroker\nmetadata: {{ name: main, namespace: org }}\n\
         spec:\n  default: true\n  image: ghcr.io/joinedcontext/antares@sha256:{DIGEST}\n  \
         size: small\n  storageClass: local-path\n  features: [temporal, ogc, mcp]\n"
    );
    assert!(registry::validate_yaml("ContextBroker", &yaml)
        .expect("a known kind")
        .is_ok());
    assert_eq!(ContextBrokerSpec::PATH_TEMPLATE, "brokers/{name}.yaml");
    assert_eq!(ContextBrokerSpec::PLURAL, "contextbrokers");
}

#[test]
fn an_image_without_a_digest_or_a_feature_twice_is_refused() {
    let mut spec = broker(true);
    for image in [
        "ghcr.io/joinedcontext/antares:0.14",
        "ghcr.io/joinedcontext/antares@sha256:abc",
        &format!("@sha256:{DIGEST}"),
        &format!("antares@sha256:{}", DIGEST.to_uppercase()),
    ] {
        spec.image = image.to_owned();
        assert!(spec.validate().is_err(), "{image} was accepted");
    }
    let twice = serde_json::from_value::<ContextBrokerSpec>(serde_json::json!({
        "image": format!("antares@sha256:{DIGEST}"),
        "features": ["ogc", "ogc"],
    }))
    .expect("parses");
    assert!(twice.validate().is_err());
    assert!(
        serde_json::from_value::<ContextBrokerSpec>(serde_json::json!({
            "image": format!("antares@sha256:{DIGEST}"),
            "replicas": 3,
        }))
        .is_err(),
        "unknown fields are refused"
    );
}

#[test]
fn a_space_lives_on_its_broker_ref_else_the_default_else_main() {
    let main = broker(true);
    let canary = broker(false);
    let brokers = || [("main", &main), ("canary", &canary)];
    assert_eq!(broker_of(None, brokers()).unwrap(), "main");
    assert_eq!(broker_of(Some("canary"), brokers()).unwrap(), "canary");
    assert!(
        broker_of(Some("gone"), brokers()).is_err(),
        "an unknown broker is an error, never the default"
    );
    assert_eq!(broker_of(None, []).unwrap(), IMPLICIT_BROKER);
    assert_eq!(broker_of(Some("main"), []).unwrap(), IMPLICIT_BROKER);
    assert!(broker_of(Some("canary"), []).is_err());
}

#[test]
fn an_organization_has_exactly_one_default_broker() {
    let yes = broker(true);
    let no = broker(false);
    assert!(
        validate_brokers([]).is_ok(),
        "none declared is the implicit main"
    );
    assert!(validate_brokers([("a", &yes), ("b", &no)]).is_ok());
    assert!(validate_brokers([("a", &no), ("b", &no)]).is_err());
    assert!(validate_brokers([("a", &yes), ("b", &yes)]).is_err());
    assert!(
        broker_of(None, [("a", &no)]).is_err(),
        "no default and no brokerRef names no broker"
    );
}

#[test]
fn a_space_names_its_broker_by_a_dns_label_and_a_space_without_one_is_unchanged() {
    let space = |extra: &str| {
        format!(
            "apiVersion: joinedcontext.com/v1alpha1\nkind: ContextSpace\n\
             metadata: {{ name: air, namespace: ovzdusie }}\nspec:\n  isSandbox: false\n{extra}"
        )
    };
    assert!(registry::validate_yaml("ContextSpace", &space(""))
        .unwrap()
        .is_ok());
    assert!(
        registry::validate_yaml("ContextSpace", &space("  brokerRef: canary\n"))
            .unwrap()
            .is_ok()
    );
    assert!(
        registry::validate_yaml("ContextSpace", &space("  brokerRef: Not_A_Label\n"))
            .unwrap()
            .is_err()
    );
}
