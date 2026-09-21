//! T-1481: the boundaries of five jc-core validators that parse what a person types.
//!
//! Each group below names its validator and walks its edges: empty, zero, the largest value, an
//! order it does not take, a digit that only looks like one, a term too short to count, an address
//! without a scheme, the same name twice.

use jc_core::envelope::ResourceEnvelope;
use jc_core::kinds::agent_profile::parse_iso_duration;
use jc_core::kinds::{DataSourceSpec, EndpointSlug, Policy, SubscriptionSpec};

// -------------------------------------------------------------------------------------------------
// parse_iso_duration (AgentProfile `limits.wallClock`)
// -------------------------------------------------------------------------------------------------

/// AG-33: a wall clock is `PT[#H][#M][#S]`, in that order, in ASCII digits.
#[test]
fn parse_iso_duration_reads_each_unit_and_their_sum() {
    assert_eq!(parse_iso_duration("PT0S"), Some(0));
    assert_eq!(parse_iso_duration("PT20M"), Some(1200));
    assert_eq!(parse_iso_duration("PT1H"), Some(3600));
    assert_eq!(parse_iso_duration("PT1H2M3S"), Some(3723));
    assert_eq!(
        parse_iso_duration("PT90S"),
        Some(90),
        "a unit past its carry is still read"
    );
}

/// AG-33: what is not a duration of that shape is no duration, never zero.
#[test]
fn parse_iso_duration_refuses_what_is_not_that_shape() {
    for raw in [
        "", "PT", "P1D", "pt20m", "20M", "PT20", "PT1S1M", "PT1M1H", "PT-1S", "PT1.5S", " PT1S",
        "PT1S ", "PT١٢S", "PT１S",
    ] {
        assert_eq!(parse_iso_duration(raw), None, "{raw:?}");
    }
}

/// AG-33: a number past `u64` used to read as `0`, and an hour count whose seconds overflow used to
/// wrap into a small number the one-hour cap then accepted.
#[test]
fn parse_iso_duration_refuses_a_total_past_the_largest_number() {
    assert_eq!(
        parse_iso_duration(&format!("PT{}S", u64::MAX)),
        Some(u64::MAX)
    );
    for raw in [
        format!("PT{}0S", u64::MAX),
        format!("PT{}H", u64::MAX / 3600 + 1),
        format!("PT{}M", u64::MAX / 60 + 1),
    ] {
        assert_eq!(parse_iso_duration(&raw), None, "{raw}");
    }
    assert_eq!(
        parse_iso_duration(&format!("PT{}M{}S", u64::MAX / 60, 60)),
        None,
        "the sum overflows even when each part does not"
    );
}

// -------------------------------------------------------------------------------------------------
// EndpointSlug::new and EndpointSlug::is_opaque
// -------------------------------------------------------------------------------------------------

const SLUG: &str = "si6epqkx364lprho5uaigutk27";

/// EP-02: a slug is at least 26 characters of lowercase base32 without padding.
#[test]
fn an_endpoint_slug_is_26_base32_characters_or_more() {
    assert!(EndpointSlug::new(SLUG).is_ok());
    assert!(EndpointSlug::new(&SLUG[..25]).is_err(), "25 characters");
    assert!(EndpointSlug::new("").is_err());
    for bad in ['1', '8', '0', '9', '=', 'A', '-', 'é'] {
        let slug = format!("{}{bad}", &SLUG[..25]);
        assert!(EndpointSlug::new(&slug).is_err(), "{slug}");
    }
}

/// EP-03: a slug that spells a forbidden term of three characters or more is not opaque, in any
/// case; a term of two characters or none is too short to count.
#[test]
fn an_opaque_slug_spells_no_term_of_three_characters() {
    let slug = EndpointSlug::new("bikesq6epqkx364lprho5uaigu").expect("a slug");
    assert!(!slug.is_opaque(&["bikes"]));
    assert!(!slug.is_opaque(&["BIK"]), "the term is compared lowercased");
    assert!(
        slug.is_opaque(&["bi"]),
        "two characters are too short to count"
    );
    assert!(slug.is_opaque(&[""]));
    assert!(slug.is_opaque(&[]));
    assert!(slug.is_opaque(&["helsinki", "air"]));
    assert!(
        !slug.is_opaque(&["helsinki", "q6e"]),
        "any one term is enough"
    );
}

// -------------------------------------------------------------------------------------------------
// NotificationEndpoint (Subscription `notification.endpoint`)
// -------------------------------------------------------------------------------------------------

const SUBSCRIPTION: &str = include_str!("golden/027-Subscription-06-configuration-as-code.yaml");
const GOLDEN_URI: &str = "uri: https://alerts.example.fi/hooks/air-quality";

fn subscription_with_uri(uri: &str) -> ResourceEnvelope<SubscriptionSpec> {
    let yaml = SUBSCRIPTION.replace(GOLDEN_URI, &format!("uri: \"{uri}\""));
    assert_ne!(
        yaml, SUBSCRIPTION,
        "the golden manifest still carries {GOLDEN_URI}"
    );
    ResourceEnvelope::from_yaml(&yaml).expect("parses")
}

/// CC-72, MF-31: the platform calls an absolute http or https URL that names a host.
#[test]
fn a_notification_endpoint_is_an_absolute_http_url_with_a_host() {
    for uri in [
        "",
        "alerts.example.fi/hooks",
        "//alerts.example.fi/hooks",
        "ftp://alerts.example.fi/hooks",
        "file:///etc/passwd",
        "https://",
        "https:///hooks",
        "https://?x=1",
    ] {
        let error = subscription_with_uri(uri).spec.validate();
        assert!(error.is_err(), "{uri:?} was accepted");
    }
    for uri in [
        "https://alerts.example.fi",
        "http://dispatcher.ovzdusie.svc:8080/hook",
        "https://alerts.example.fi/hooks?source=jc",
    ] {
        subscription_with_uri(uri)
            .spec
            .validate()
            .unwrap_or_else(|err| panic!("{uri}: {err}"));
    }
}

/// MF-31: a credential parameter is refused in any case, and a parameter that only starts like one
/// is not.
#[test]
fn a_credential_query_parameter_is_refused_in_any_case() {
    for uri in [
        "https://alerts.example.fi/hooks?ACCESS_TOKEN=s3cr3t-value",
        "https://alerts.example.fi/hooks?a=1&api_key=s3cr3t-value",
        "https://alerts.example.fi/hooks?apikey",
    ] {
        let error = subscription_with_uri(uri)
            .spec
            .validate()
            .expect_err(uri)
            .to_string();
        assert!(!error.contains("s3cr3t-value"), "{error}");
    }
    subscription_with_uri("https://alerts.example.fi/hooks?api_keys_page=2&&")
        .spec
        .validate()
        .expect("a parameter named like no credential");
}

// -------------------------------------------------------------------------------------------------
// DataSourceSpec::validate_runner (a runner-typed DataSource)
// -------------------------------------------------------------------------------------------------

const KAFKA: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: DataSource
metadata:
  name: kafka-sensors
  namespace: bb-ovzdusie
spec:
  type: kafka
  input:
    addresses: ["kafka.banskabystrica.sk:9092"]
    topics: ["sensors.aq"]
    sasl:
      password: ${DS_KAFKA_PASSWORD}
  secrets:
    - name: kafka-secret
      key: password
      envVar: DS_KAFKA_PASSWORD
"#;

fn kafka(yaml: &str) -> Result<(), String> {
    let source: ResourceEnvelope<DataSourceSpec> =
        ResourceEnvelope::from_yaml(yaml).map_err(|err| err.to_string())?;
    source.validate().map_err(|err| err.to_string())
}

/// MF-35, PL-16: a runner's input is a non-empty mapping, and each secret it names is one
/// `${VAR}` of an entry of `spec.secrets` whose `envVar` is an environment variable name.
#[test]
fn a_runner_input_names_each_secret_by_one_declared_variable() {
    kafka(KAFKA).expect("the example validates");
    let cases = [
        ("an empty input", KAFKA.replace(
            "  input:\n    addresses: [\"kafka.banskabystrica.sk:9092\"]\n    topics: [\"sensors.aq\"]\n    sasl:\n      password: ${DS_KAFKA_PASSWORD}\n",
            "  input: {}\n",
        )),
        ("a variable nobody declared", KAFKA.replace("password: ${DS_KAFKA_PASSWORD}", "password: ${DS_OTHER}")),
        ("a variable inside a longer value", KAFKA.replace("password: ${DS_KAFKA_PASSWORD}", "password: \"x${DS_KAFKA_PASSWORD}\"")),
        ("an empty value", KAFKA.replace("password: ${DS_KAFKA_PASSWORD}", "password: \"\"")),
        ("a lowercase envVar", KAFKA.replace("envVar: DS_KAFKA_PASSWORD", "envVar: ds_kafka_password")),
        ("no envVar", KAFKA.replace("      envVar: DS_KAFKA_PASSWORD\n", "")),
    ];
    for (what, yaml) in cases {
        assert_ne!(yaml, KAFKA, "{what}: the example changed under the test");
        assert!(kafka(&yaml).is_err(), "{what} was accepted");
    }
}

// -------------------------------------------------------------------------------------------------
// The duplicate checks of a Policy's `information` (seen_props, seen_rels)
// -------------------------------------------------------------------------------------------------

const POLICY: &str = include_str!("golden/006-Policy-03-domain-model.yaml");

fn policy(yaml: &str) -> Result<(), String> {
    let policy: Policy = ResourceEnvelope::from_yaml(yaml).map_err(|err| err.to_string())?;
    policy.spec.validate().map_err(|err| err.to_string())
}

/// R6: a property or relationship named twice, or named as blank, is refused, and the error
/// names the list and the name.
#[test]
fn an_information_entry_names_each_property_and_relationship_once() {
    policy(POLICY).expect("the golden policy validates");
    let props = "propertyNames: [pm10, pm25, dateObserved, location]";
    let rels = "relationshipNames: [refDistrict]";
    for (what, yaml, field) in [
        (
            "a property twice",
            POLICY.replace(props, "propertyNames: [pm10, pm25, pm10]"),
            "propertyNames",
        ),
        (
            "a blank property",
            POLICY.replace(props, "propertyNames: [pm10, \" \"]"),
            "propertyNames",
        ),
        (
            "a relationship twice",
            POLICY.replace(rels, "relationshipNames: [refDistrict, refDistrict]"),
            "relationshipNames",
        ),
        (
            "a blank relationship",
            POLICY.replace(rels, "relationshipNames: [\"\"]"),
            "relationshipNames",
        ),
    ] {
        assert_ne!(
            yaml, POLICY,
            "{what}: the golden policy changed under the test"
        );
        let error = policy(&yaml).expect_err(what);
        assert!(error.contains(field), "{what}: {error}");
    }
    // Names differ by case in NGSI-LD, so two spellings are two names.
    policy(&POLICY.replace(props, "propertyNames: [pm10, PM10]")).expect("two spellings");
}
