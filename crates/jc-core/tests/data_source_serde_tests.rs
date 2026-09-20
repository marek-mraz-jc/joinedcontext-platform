//! T-0290: the `DataSource` kind, one connection and its references (MF-35, PL-39, CC-06).

use jc_core::envelope::ResourceEnvelope;
use jc_core::kinds::{DataSourceSpec, DataSourceType, GtfsFeed, PipelineClass, PipelineSpec};
use jc_core::registry;
use serde::de::Error as _;

type DataSource = ResourceEnvelope<DataSourceSpec>;

const MQTT: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: DataSource
metadata:
  name: mqtt-mesto
  namespace: bb-ovzdusie
  title: { sk: "Mestský MQTT broker", en: "City MQTT broker" }
spec:
  type: mqtt
  mqtt:
    urls: ["tls://mqtt.banskabystrica.sk:8883"]
    topics: ["sensors/aq/+/reading"]
    qos: 1
    cleanSession: false
    username: bb-collector
    passwordRef: { name: mqtt-mesto, key: password }
  tls:
    caCertRef: { name: city-ca, key: ca.crt }
"#;

const HTTP: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: DataSource
metadata:
  name: aq-opendata
  namespace: bb-ovzdusie
spec:
  type: http
  http:
    url: https://opendata.banskabystrica.sk/aq.json
    verb: GET
    headers: { Accept: application/json }
    authorization: { scheme: Bearer, headerRef: { name: aq-api, key: token } }
    timeout: 10s
"#;

const WEB_SOCKET: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: DataSource
metadata:
  name: aq-stream
  namespace: bb-ovzdusie
spec:
  type: websocket
  webSocket:
    url: wss://feed.banskabystrica.sk/aq
    openMessage: '{"subscribe":"aq"}'
"#;

const GTFS_RT: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: DataSource
metadata:
  name: mhd-vehicles
  namespace: bb-doprava
spec:
  type: gtfs-rt
  gtfsRt:
    url: https://gtfs.banskabystrica.sk/vehiclepositions.pb
    feed: vehiclePositions
"#;

/// Every variant survives the round trip byte for byte in its meaning: what a reviewer read in
/// Git is what the reconciler renders from, and a field the parser drops silently is a change
/// nobody approved.
#[test]
fn every_connection_type_round_trips() {
    for (name, text, expected) in [
        ("mqtt", MQTT, DataSourceType::Mqtt),
        ("http", HTTP, DataSourceType::Http),
        ("webSocket", WEB_SOCKET, DataSourceType::WebSocket),
        ("gtfsRt", GTFS_RT, DataSourceType::GtfsRt),
    ] {
        let parsed = DataSource::from_yaml(text).unwrap_or_else(|e| panic!("{name}: {e}"));
        parsed.validate().unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(parsed.spec.source_type, expected);

        let again = DataSource::from_yaml(&serde_norway::to_string(&parsed).expect("serialize"))
            .unwrap_or_else(|e| panic!("{name} re-parse: {e}"));
        assert_eq!(parsed, again, "{name} does not survive a round trip");
    }
}

/// T-0393: the wire value is `websocket`, one word, and `web-socket` is not an alias.
///
/// The kind is documented before it is written (CC-11), so a manifest copied out of
/// `Architecture/08 §6` has to parse. Keeping the kebab-case spelling alive as a second
/// accepted value would be the friendlier change and the wrong one: two spellings for one type
/// is two things to write in every Portal form, every example and every locale bundle, and the
/// day they disagree is the day a manifest means different things to the reconciler and the UI.
#[test]
fn the_websocket_type_is_one_word() {
    let parsed = DataSource::from_yaml(WEB_SOCKET).expect("the documented spelling parses");
    parsed.validate().expect("valid");
    assert_eq!(parsed.spec.source_type, DataSourceType::WebSocket);
    assert_eq!(DataSourceType::WebSocket.to_string(), "websocket");
    assert!(
        serde_norway::to_string(&parsed)
            .expect("serialize")
            .contains("type: websocket"),
        "what is written back is what the documentation shows"
    );

    let kebab = WEB_SOCKET.replace("type: websocket", "type: web-socket");
    let err = DataSource::from_yaml(&kebab)
        .and_then(|d| {
            d.validate()
                .map_err(|e| serde_norway::Error::custom(e.to_string()))
        })
        .expect_err("the old kebab-case spelling is refused, not silently accepted");
    assert!(err.to_string().contains("web-socket") || err.to_string().contains("type"));
}

#[test]
fn the_mqtt_connection_keeps_every_field_it_was_given() {
    let source = DataSource::from_yaml(MQTT).expect("parses");
    let mqtt = source.spec.mqtt.as_ref().expect("mqtt block");
    assert_eq!(mqtt.urls, ["tls://mqtt.banskabystrica.sk:8883"]);
    assert_eq!(mqtt.topics, ["sensors/aq/+/reading"]);
    assert_eq!(mqtt.qos, Some(1));
    assert_eq!(mqtt.clean_session, Some(false));
    assert_eq!(mqtt.username.as_deref(), Some("bb-collector"));
    assert_eq!(
        mqtt.password_ref.as_ref().map(|r| r.name.as_str()),
        Some("mqtt-mesto")
    );
    assert_eq!(source.spec.secret_refs().len(), 2, "password and CA");
}

#[test]
fn the_gtfs_feed_names_which_of_the_three_it_is() {
    let source = DataSource::from_yaml(GTFS_RT).expect("parses");
    assert_eq!(
        source.spec.gtfs_rt.as_ref().map(|g| g.feed),
        Some(GtfsFeed::VehiclePositions)
    );
    assert!(
        source.spec.secret_refs().is_empty(),
        "a public feed has none"
    );
}

/// CC-06: the type has no member a password could be written into, so the plaintext version of
/// the manifest above does not parse at all. This is the security property of MF-35.
#[test]
fn a_plaintext_credential_is_not_a_field_that_exists() {
    let plaintext = MQTT.replace(
        "passwordRef: { name: mqtt-mesto, key: password }",
        "password: hunter2",
    );
    let error = DataSource::from_yaml(&plaintext).expect_err("plaintext password is refused");
    assert!(
        error.to_string().contains("password"),
        "the error should name the offending field, got: {error}"
    );
}

#[test]
fn the_block_has_to_be_the_one_the_type_names() {
    let wrong = MQTT.replace("type: mqtt", "type: http");
    let error = DataSource::from_yaml(&wrong)
        .expect("parses")
        .validate()
        .expect_err("a type without its block is refused");
    assert!(error.to_string().contains("type"), "{error}");

    let both = MQTT.replace(
        "  tls:",
        "  http:\n    url: https://example.sk/a.json\n  tls:",
    );
    assert!(
        DataSource::from_yaml(&both)
            .expect("parses")
            .validate()
            .is_err(),
        "two connections in one source is refused"
    );
}

#[test]
fn a_connection_cannot_point_at_a_scheme_it_does_not_speak() {
    let file = MQTT.replace("tls://mqtt.banskabystrica.sk:8883", "file:///etc/passwd");
    assert!(DataSource::from_yaml(&file)
        .expect("parses")
        .validate()
        .is_err());

    let broker = HTTP.replace(
        "https://opendata.banskabystrica.sk/aq.json",
        "tcp://mqtt.banskabystrica.sk:1883",
    );
    let error = DataSource::from_yaml(&broker)
        .expect("parses")
        .validate()
        .expect_err("a broker scheme on an http connection is refused");
    // T-1194: this check was proposed for merging with `sync`'s `validate_remote_url` into one
    // shared scheme validator. What a data source needs to say is that the connection type does
    // not speak the scheme — plaintext is not the question here, `http://` is a legal one. A
    // shared function returns a shared reason, and one of the two would then be wrong.
    assert!(
        error
            .to_string()
            .contains("not one this connection type speaks"),
        "{error}"
    );
    let plaintext = HTTP.replace("https://opendata", "http://opendata");
    DataSource::from_yaml(&plaintext)
        .expect("parses")
        .validate()
        .expect("an http data source is a legal one, unlike a sync origin");
}

#[test]
fn tls_verification_is_not_something_a_manifest_can_turn_off() {
    let insecure = MQTT.replace(
        "  tls:\n    caCertRef: { name: city-ca, key: ca.crt }",
        "  tls:\n    insecureSkipVerify: true",
    );
    let error = DataSource::from_yaml(&insecure)
        .expect("parses")
        .validate()
        .expect_err("skipping verification is refused");
    assert!(error.to_string().contains("caCertRef"), "{error}");
}

#[test]
fn the_environment_variable_of_a_reference_is_derived_or_stated() {
    let source = DataSource::from_yaml(MQTT).expect("parses");
    let password = source.spec.secret_refs()[0];
    assert_eq!(
        jc_core::kinds::data_source::env_var_of("mqtt-mesto", password),
        "DS_MQTT_MESTO_PASSWORD"
    );

    let stated = HTTP.replace(
        "headerRef: { name: aq-api, key: token }",
        "headerRef: { name: aq-api, key: token, envVar: AQ_TOKEN }",
    );
    let source = DataSource::from_yaml(&stated).expect("parses");
    assert_eq!(
        jc_core::kinds::data_source::env_var_of("aq-opendata", source.spec.secret_refs()[0]),
        "AQ_TOKEN"
    );
}

/// MF-09: the kind is in the catalogue, so the resource API, `jcctl` and the Portal form all
/// see it without another line of code anywhere.
#[test]
fn the_kind_is_in_the_catalogue_with_a_draft07_schema() {
    let info = registry::by_kind("DataSource").expect("catalogued");
    assert_eq!(info.plural, "datasources");
    assert_eq!(
        info.repo_path("bb-ovzdusie", "ovzdusie", "mqtt-mesto"),
        "projects/bb-ovzdusie/datasources/mqtt-mesto.yaml"
    );
    assert_eq!(registry::by_plural("datasources"), Some(info));

    let schema = registry::schema_of("DataSource").expect("schema");
    assert_eq!(
        schema.get("$schema").and_then(|v| v.as_str()),
        Some("http://json-schema.org/draft-07/schema#")
    );
    assert!(registry::validate_yaml("DataSource", MQTT)
        .expect("known kind")
        .is_ok());
}

/// PL-39: the reference is the pipeline's input, so it excludes the other input.
#[test]
fn a_pipeline_reads_a_data_source_or_an_endpoint_but_not_both() {
    let one_input = r#"class: resident
source:
  dataSourceRef: { kind: DataSource, name: mqtt-mesto }
targetEndpoint: urn:ngsi-ld:Endpoint:banskabystrica.sk:ovzdusie:ep-air
"#;
    let spec: PipelineSpec = serde_norway::from_str(one_input).expect("parses");
    spec.validate().expect("one input is valid");
    assert_eq!(
        spec.source
            .as_ref()
            .and_then(|s| s.data_source_ref.as_ref())
            .map(|r| r.name()),
        Some("mqtt-mesto")
    );

    let two_inputs = one_input.replace(
        "  dataSourceRef: { kind: DataSource, name: mqtt-mesto }",
        "  dataSourceRef: { kind: DataSource, name: mqtt-mesto }\n  endpointRef: { kind: Endpoint, name: ovzdusie-internal }",
    );
    let spec: PipelineSpec = serde_norway::from_str(&two_inputs).expect("parses");
    assert!(spec.validate().is_err(), "two inputs is refused");

    let wrong_kind = one_input.replace("kind: DataSource", "kind: Endpoint");
    let spec: PipelineSpec = serde_norway::from_str(&wrong_kind).expect("parses");
    assert!(spec.validate().is_err(), "the reference names its kind");
}

/// T-0321, MF-35: an API key in a vendor header is the common shape of an open-data feed. The
/// header is named in the manifest and the credential still comes from the secret store, so the
/// feed is reachable without writing the key into `headers` as a value.
#[test]
fn a_credential_may_name_the_header_it_is_written_into() {
    let source = DataSource::from_yaml(HTTP).expect("parses");
    let authorization = source.spec.http.as_ref().unwrap().authorization.as_ref();
    let authorization = authorization.expect("the fixture authorizes");
    assert_eq!(authorization.header_name(), "Authorization");
    assert_eq!(authorization.scheme_prefix(), "Bearer");

    let vendor = HTTP.replace(
        "authorization: { scheme: Bearer, headerRef: { name: aq-api, key: token } }",
        "authorization: { header: digitransit-subscription-key, headerRef: { name: aq-api, key: key } }",
    );
    let source = DataSource::from_yaml(&vendor).expect("parses");
    source.validate().expect("a vendor header is a header");
    let authorization = source.spec.http.as_ref().unwrap().authorization.clone();
    let authorization = authorization.expect("the fixture authorizes");
    assert_eq!(authorization.header_name(), "digitransit-subscription-key");
    // A key header takes the bare key: `Bearer ` in front of it is what the feed refuses.
    assert_eq!(authorization.scheme_prefix(), "");
}

/// A header name that is not a token could carry a second header or a value past the colon
/// into the rendered config, so it is refused where every other name is (MF-35).
#[test]
fn a_header_name_that_is_not_a_token_is_refused() {
    for bad in ["x-key: injected\r\nX-Other", "x key", "", "x-key:"] {
        let yaml = HTTP.replace(
            "authorization: { scheme: Bearer, headerRef: { name: aq-api, key: token } }",
            &format!(
                "authorization: {{ header: \"{bad}\", headerRef: {{ name: aq-api, key: key }} }}"
            ),
        );
        let source = DataSource::from_yaml(&yaml).expect("parses");
        assert!(
            source.validate().is_err(),
            "{bad:?} is not a header name a manifest may write"
        );
    }
}

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

#[test]
fn runner_kafka_with_valid_interpolation_and_secrets_validates() {
    let source = DataSource::from_yaml(KAFKA).expect("parses");
    source.validate().expect("validates");
    assert_eq!(
        source.spec.source_type,
        DataSourceType::Runner("kafka".to_string())
    );

    // Wire round-trip verifies type: kafka serializes as "kafka" string
    let serialized = serde_norway::to_string(&source).expect("serializes");
    assert!(serialized.contains("type: kafka"));
    let again = DataSource::from_yaml(&serialized).expect("re-parses");
    assert_eq!(source, again);

    // secret_refs lists every entry of spec.secrets
    let refs = source.spec.secret_refs();
    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0].name, "kafka-secret");
    assert_eq!(refs[0].env_var.as_deref(), Some("DS_KAFKA_PASSWORD"));
}

#[test]
fn runner_kafka_with_literal_password_is_refused() {
    let literal = KAFKA.replace("${DS_KAFKA_PASSWORD}", "plaintext_pass");
    let source = DataSource::from_yaml(&literal).expect("parses");
    let err = source.validate().expect_err("literal password refused");
    assert!(
        err.to_string().contains("spec.input.sasl.password"),
        "error should name spec.input.sasl.password, got: {err}"
    );
}

#[test]
fn runner_client_certs_key_literal_in_element_1_is_refused() {
    let certs_yaml = r#"apiVersion: joinedcontext.com/v1alpha1
kind: DataSource
metadata:
  name: kafka-certs
  namespace: bb-ovzdusie
spec:
  type: kafka
  input:
    addresses: ["kafka.banskabystrica.sk:9092"]
    tls:
      client_certs:
        - key: ${DS_CERT0_KEY}
        - key: literal_key_in_elem_1
  secrets:
    - name: cert-0
      key: key
      envVar: DS_CERT0_KEY
"#;
    let source = DataSource::from_yaml(certs_yaml).expect("parses");
    let err = source
        .validate()
        .expect_err("literal key in element 1 refused");
    assert!(
        err.to_string()
            .contains("spec.input.tls.client_certs[1].key"),
        "error should name spec.input.tls.client_certs[1].key, got: {err}"
    );
}

#[test]
fn runner_input_reading_the_runners_own_environment_is_refused() {
    for (needle, replacement) in [
        ("kafka.banskabystrica.sk", "${JC_CLIENT_SECRET}.example"),
        ("kafka.banskabystrica.sk", "${JC_CLIENT_SECRET:fallback}"),
        (
            "kafka.banskabystrica.sk",
            "x${DS_KAFKA_PASSWORD}${JC_CLIENT_SECRET}",
        ),
    ] {
        assert!(KAFKA.contains(needle), "fixture holds {needle}");
        let leaking = KAFKA.replace(needle, replacement);
        let source = DataSource::from_yaml(&leaking).expect("parses");
        let err = source
            .validate()
            .expect_err("undeclared interpolation refused");
        assert!(
            err.to_string().contains("${JC_CLIENT_SECRET}"),
            "error names the variable, got: {err}"
        );
    }
    let declared = KAFKA.replace("kafka.banskabystrica.sk", "${DS_KAFKA_PASSWORD}.example");
    DataSource::from_yaml(&declared)
        .expect("parses")
        .validate()
        .expect("a declared secret may appear outside its field");
}

fn sql_select(dsn: &str) -> DataSource {
    DataSource::from_yaml(&format!(
        r#"apiVersion: joinedcontext.com/v1alpha1
kind: DataSource
metadata:
  name: counters-db
  namespace: bb-ovzdusie
spec:
  type: sql_select
  input:
    driver: postgres
    dsn: "{dsn}"
    table: counters
    columns: ["*"]
  secrets:
    - name: counters-db
      key: password
      envVar: DS_COUNTERS_DB_PASSWORD
"#
    ))
    .expect("parses")
}

#[test]
fn a_literal_password_inside_a_connection_string_is_refused() {
    for dsn in [
        "postgres://jc:hunter2@db:5432/counters",
        "jc:hunter2@tcp(db:3306)/counters",
        "sqlserver://db:1433?database=counters&password=hunter2",
    ] {
        let err = sql_select(dsn)
            .validate()
            .expect_err("a literal password is refused");
        assert!(
            err.to_string().contains("spec.input.dsn"),
            "{dsn}: error names the field, got: {err}"
        );
    }
    for dsn in [
        "postgres://jc:${DS_COUNTERS_DB_PASSWORD}@db:5432/counters",
        "sqlserver://db:1433?database=counters&password=${DS_COUNTERS_DB_PASSWORD}",
        "file:/data/counters.db",
        "postgres://db:5432/counters",
    ] {
        sql_select(dsn)
            .validate()
            .unwrap_or_else(|e| panic!("{dsn}: {e}"));
    }
}

#[test]
fn unknown_type_nope_is_refused_and_names_accepted_types() {
    let nope = KAFKA.replace("type: kafka", "type: nope");
    let source = DataSource::from_yaml(&nope).expect("parses");
    let err = source.validate().expect_err("type nope refused");
    let msg = err.to_string();
    assert!(
        msg.contains("kafka") && msg.contains("mqtt"),
        "error reason must contain kafka and mqtt, got: {msg}"
    );
}

#[test]
fn file_reader_paths_must_start_with_data_and_have_no_dotdot() {
    let csv_bad_path = r#"apiVersion: joinedcontext.com/v1alpha1
kind: DataSource
metadata:
  name: csv-feed
  namespace: bb-ovzdusie
spec:
  type: csv
  input:
    paths: ["/tmp/x.csv"]
"#;
    let source = DataSource::from_yaml(csv_bad_path).expect("parses");
    let err = source.validate().expect_err("/tmp path refused");
    assert!(err.to_string().contains("spec.input.paths"), "{err}");

    let csv_valid = csv_bad_path.replace("/tmp/x.csv", "/data/x.csv");
    let source_ok = DataSource::from_yaml(&csv_valid).expect("parses");
    source_ok.validate().expect("/data/x.csv accepted");

    let csv_dotdot = csv_bad_path.replace("/tmp/x.csv", "/data/../x.csv");
    let source_dotdot = DataSource::from_yaml(&csv_dotdot).expect("parses");
    let err = source_dotdot.validate().expect_err(".. refused");
    assert!(err.to_string().contains("spec.input.paths"), "{err}");
}

#[test]
fn typed_mqtt_with_input_and_runner_with_tls_are_refused() {
    let mqtt_with_input = MQTT.to_string() + "  input:\n    some: field\n";
    let source = DataSource::from_yaml(&mqtt_with_input).expect("parses");
    let err = source
        .validate()
        .expect_err("typed mqtt with input is refused");
    assert!(err.to_string().contains("spec.input"), "{err}");

    let kafka_with_tls = KAFKA.to_string() + "  tls:\n    insecureSkipVerify: false\n";
    let source = DataSource::from_yaml(&kafka_with_tls).expect("parses");
    let err = source
        .validate()
        .expect_err("runner type with tls is refused");
    assert!(err.to_string().contains("spec.tls"), "{err}");
}

#[test]
fn terminates_distinguishes_terminating_and_continuous_inputs() {
    let mut ds: DataSourceSpec = serde_norway::from_str(
        r#"type: csv
input:
  paths: ["/data/x.csv"]
"#,
    )
    .unwrap();
    assert!(ds.terminates(), "csv terminates");

    ds.source_type = DataSourceType::Runner("sql_select".to_string());
    assert!(ds.terminates(), "sql_select terminates");

    ds.source_type = DataSourceType::Http;
    assert!(ds.terminates(), "http terminates");

    ds.source_type = DataSourceType::Runner("kafka".to_string());
    assert!(!ds.terminates(), "kafka does not terminate");

    ds.source_type = DataSourceType::Mqtt;
    assert!(!ds.terminates(), "mqtt does not terminate");
}

#[test]
fn check_class_enforces_resident_for_continuous_sources() {
    let mut pipe: PipelineSpec = serde_norway::from_str(
        r#"class: scheduled
schedule: "0 0 * * *"
targetEndpoint: urn:ngsi-ld:Endpoint:banskabystrica.sk:ovzdusie:ep-air
"#,
    )
    .unwrap();

    let mut ds: DataSourceSpec = serde_norway::from_str(
        r#"type: kafka
input:
  addresses: ["localhost:9092"]
"#,
    )
    .unwrap();

    // scheduled + kafka -> refused
    assert!(jc_core::kinds::data_source::check_class(&pipe, &ds).is_err());

    // scheduled + csv -> ok
    ds.source_type = DataSourceType::Runner("csv".to_string());
    assert!(jc_core::kinds::data_source::check_class(&pipe, &ds).is_ok());

    // resident + kafka -> ok
    pipe.class = PipelineClass::Resident;
    pipe.schedule = None;
    ds.source_type = DataSourceType::Runner("kafka".to_string());
    assert!(jc_core::kinds::data_source::check_class(&pipe, &ds).is_ok());

    // auto + period 5m + nats -> refused
    pipe.class = PipelineClass::Auto;
    pipe.period = Some("5m".to_string());
    ds.source_type = DataSourceType::Runner("nats".to_string());
    assert!(jc_core::kinds::data_source::check_class(&pipe, &ds).is_err());
}

#[test]
fn datasource_schema_spec_type_is_string_with_examples_and_no_enum() {
    let schema = registry::schema_of("DataSource").expect("schema");
    assert_eq!(
        schema.get("$schema").and_then(|v| v.as_str()),
        Some("http://json-schema.org/draft-07/schema#")
    );

    let type_schema = schema
        .pointer("/definitions/DataSourceType")
        .or_else(|| schema.pointer("/properties/spec/properties/type"))
        .expect("DataSourceType schema definition");

    assert_eq!(
        type_schema.get("type").and_then(|v| v.as_str()),
        Some("string"),
        "spec.type schema must be type: string"
    );
    assert!(
        type_schema.get("enum").is_none(),
        "spec.type must not be an enum of 69 names"
    );
    let examples = type_schema
        .get("examples")
        .and_then(|v| v.as_array())
        .expect("spec.type must have examples");
    assert!(examples.iter().any(|v| v == "kafka"));
    assert!(examples.iter().any(|v| v == "mqtt"));

    let schema_text = schema.to_string();
    for disallowed in [
        "prefixItems",
        "dependentRequired",
        "unevaluatedProperties",
        "$defs",
    ] {
        assert!(
            !schema_text.contains(disallowed),
            "schema must not contain 2019-09 keyword `{disallowed}`"
        );
    }
}
