//! T-0291: what a `DataSource` becomes in the generated `bento.yaml` (PL-39, PL-16, PL-03).
//!
//! The golden strings below are the contract with the runner: Bento's own field names, the
//! credential as an interpolation and never a value, and the author's processors and output
//! exactly as they were written.

use jc_core::envelope::ResourceEnvelope;
use jc_core::kinds::{DataSourceSpec, PipelineSpec};
use jc_manifest::bento::{input_of, render, InputContext, RenderError};

fn source(yaml: &str) -> DataSourceSpec {
    let parsed = ResourceEnvelope::<DataSourceSpec>::from_yaml(yaml).expect("parses");
    parsed.validate().expect("valid");
    parsed.spec
}

fn context<'a>(name: &'a str) -> InputContext<'a> {
    InputContext {
        source: name,
        project: "bb-ovzdusie",
        pipeline: "aq-mqtt-ingest",
        pipeline_spec: None,
    }
}

fn rendered(spec: &DataSourceSpec, name: &str) -> String {
    serde_norway::to_string(&input_of(spec, &context(name))).expect("serializes")
}

const MQTT: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: DataSource
metadata: { name: mqtt-mesto, namespace: bb-ovzdusie }
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

const MQTT_INPUT: &str = r#"mqtt:
  urls:
  - tls://mqtt.banskabystrica.sk:8883
  topics:
  - sensors/aq/+/reading
  client_id: jc-bb-ovzdusie-aq-mqtt-ingest-${JC_MQTT_NAMESPACE}
  qos: 1
  clean_session: false
  user: bb-collector
  password: ${DS_MQTT_MESTO_PASSWORD}
  tls:
    enabled: true
    root_cas: ${DS_MQTT_MESTO_CA_CRT}
"#;

const HTTP: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: DataSource
metadata: { name: aq-opendata, namespace: bb-ovzdusie }
spec:
  type: http
  http:
    url: https://opendata.banskabystrica.sk/aq.json
    headers: { Accept: application/json }
    authorization: { scheme: Bearer, headerRef: { name: aq-api, key: token } }
    timeout: 10s
"#;

const HTTP_INPUT: &str = r#"http_client:
  url: https://opendata.banskabystrica.sk/aq.json
  verb: GET
  headers:
    Accept: application/json
    Authorization: Bearer ${DS_AQ_OPENDATA_TOKEN}
  timeout: 10s
  tls:
    enabled: true
"#;

const WEB_SOCKET: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: DataSource
metadata: { name: aq-stream, namespace: bb-ovzdusie }
spec:
  type: websocket
  webSocket:
    url: wss://feed.banskabystrica.sk/aq
    openMessage: '{"subscribe":"aq"}'
"#;

const WEB_SOCKET_INPUT: &str = r#"websocket:
  url: wss://feed.banskabystrica.sk/aq
  open_message: '{"subscribe":"aq"}'
  open_message_type: text
"#;

const GTFS_RT: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: DataSource
metadata: { name: mhd-vehicles, namespace: bb-doprava }
spec:
  type: gtfs-rt
  gtfsRt:
    url: https://gtfs.banskabystrica.sk/vehiclepositions.pb
    feed: vehiclePositions
"#;

/// What the author wrote: the meaning of the payload, and nothing about where it comes from.
const AUTHORED: &str = r#"pipeline:
  processors:
  - mapping: |
      let domain = env("JC_ORG_DOMAIN")
      root.id = "urn:ngsi-ld:%v:%v:%v:%v".format("AirQualityObserved", $domain, "ovzdusie", this.station)
output:
  http_client:
    url: ${JC_ENDPOINT_URL}/entityOperations/upsert
    verb: POST
"#;

#[test]
fn an_mqtt_connection_becomes_bentos_mqtt_input() {
    assert_eq!(rendered(&source(MQTT), "mqtt-mesto"), MQTT_INPUT);
}

/// PL-76: a broker of the platform is read on its internal listener as the runner, with no
/// credential and no TLS block, the namespace filled by the runner.
#[test]
fn a_platform_broker_is_read_on_its_internal_listener_without_a_credential() {
    const BROKER: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: DataSource
metadata: { name: cameras, namespace: bb-ovzdusie }
spec:
  type: mqtt
  mqtt:
    brokerRef: { kind: MqttBroker, name: cameras }
    topics: ["cameras/+/frame"]
"#;
    assert_eq!(
        rendered(&source(BROKER), "cameras"),
        r#"mqtt:
  urls:
  - tcp://mqtt-bb-ovzdusie-cameras.${JC_MQTT_NAMESPACE}.svc.cluster.local:1883
  topics:
  - cameras/+/frame
  client_id: jc-bb-ovzdusie-aq-mqtt-ingest
  user: pipeline-runner
"#
    );
}

#[test]
fn an_http_connection_becomes_an_http_client_poll() {
    assert_eq!(rendered(&source(HTTP), "aq-opendata"), HTTP_INPUT);
}

#[test]
fn a_websocket_connection_becomes_a_websocket_input() {
    assert_eq!(rendered(&source(WEB_SOCKET), "aq-stream"), WEB_SOCKET_INPUT);
}

/// PL-16: the generated config carries the interpolation and the manifest carries the
/// reference, so the secret itself exists in neither.
#[test]
fn no_rendered_input_contains_a_credential() {
    for (yaml, name) in [(MQTT, "mqtt-mesto"), (HTTP, "aq-opendata")] {
        let input = rendered(&source(yaml), name);
        assert!(input.contains("${DS_"), "no interpolation in:\n{input}");
        for leak in ["hunter2", "secret", "password: ", "token: "] {
            assert!(
                !input.contains(&format!("{leak}bb")) && !input.contains("Bearer eyJ"),
                "{name} leaked something that looks like a credential:\n{input}"
            );
        }
    }
}

#[test]
fn the_authors_processors_and_output_come_back_untouched() {
    let out = render(AUTHORED, &source(MQTT), &context("mqtt-mesto")).expect("renders");
    assert!(out.starts_with("input:"), "the input comes first:\n{out}");
    assert!(out.contains("env(\"JC_ORG_DOMAIN\")"));
    assert!(out.contains(
        "\"urn:ngsi-ld:%v:%v:%v:%v\".format(\"AirQualityObserved\", $domain, \"ovzdusie\", this.station)"
    ));
    assert!(out.contains("${JC_ENDPOINT_URL}/entityOperations/upsert"));

    // The author's half survives a round trip through the renderer unchanged.
    let parsed: serde_norway::Value = serde_norway::from_str(&out).expect("parses");
    let authored: serde_norway::Value = serde_norway::from_str(AUTHORED).expect("parses");
    for key in ["pipeline", "output"] {
        assert_eq!(
            parsed.get(key),
            authored.get(key),
            "{key} changed while rendering the input"
        );
    }
}

/// The GTFS-realtime feed is the one type that contributes a processor: the protobuf decoder
/// runs before the author's mapping, which then sees plain JSON.
#[test]
fn the_gtfs_decoder_runs_before_the_authors_processors() {
    let out = render(AUTHORED, &source(GTFS_RT), &context("mhd-vehicles")).expect("renders");
    let parsed: serde_norway::Value = serde_norway::from_str(&out).expect("parses");
    let processors = parsed
        .get("pipeline")
        .and_then(|p| p.get("processors"))
        .and_then(|p| p.as_sequence())
        .expect("processors");
    assert_eq!(processors.len(), 2, "decoder plus the author's mapping");
    assert_eq!(
        processors[0]
            .get("protobuf")
            .and_then(|p| p.get("message"))
            .and_then(|m| m.as_str()),
        Some("transit_realtime.FeedMessage")
    );
    assert!(
        processors[1].get("mapping").is_some(),
        "the author's is second"
    );
}

/// PL-39: two inputs in one config is a merge nobody can review, so the plan fails instead.
#[test]
fn an_authored_input_and_a_reference_cannot_both_stand() {
    let with_input = format!("input:\n  generate:\n    interval: 10s\n{AUTHORED}");
    assert_eq!(
        render(&with_input, &source(MQTT), &context("mqtt-mesto")),
        Err(RenderError::InputAlreadyDeclared)
    );
}

#[test]
fn a_pipeline_without_a_bento_file_yet_still_renders() {
    let out = render("", &source(WEB_SOCKET), &context("aq-stream")).expect("renders");
    assert!(out.starts_with("input:"), "{out}");
    assert!(out.contains("wss://feed.banskabystrica.sk/aq"));
}

#[test]
fn a_bento_file_that_is_not_a_mapping_is_refused_by_name() {
    assert_eq!(
        render("- one\n- two\n", &source(MQTT), &context("mqtt-mesto")),
        Err(RenderError::NotAMapping)
    );
    assert!(matches!(
        render("input: [\n", &source(MQTT), &context("mqtt-mesto")),
        Err(RenderError::Parse(_))
    ));
}

/// T-0321, MF-35: a feed whose key goes in a vendor header renders that header with the bare
/// interpolation. `Bearer ` in front of an API key is what such a feed refuses, so the scheme
/// is written only where there is one.
#[test]
fn a_credential_that_names_its_header_renders_that_header_without_a_scheme() {
    let vendor = HTTP.replace(
        "authorization: { scheme: Bearer, headerRef: { name: aq-api, key: token } }",
        "authorization: { header: digitransit-subscription-key, headerRef: { name: aq-api, key: key } }",
    );
    let input = rendered(&source(&vendor), "hsl-gbfs");
    assert!(
        input.contains("digitransit-subscription-key: ${DS_HSL_GBFS_KEY}"),
        "the vendor header carries the bare interpolation:\n{input}"
    );
    assert!(
        !input.contains("Authorization:"),
        "nothing writes an Authorization header the manifest never asked for:\n{input}"
    );

    // An explicit scheme still wins, on any header.
    let signed = HTTP.replace(
        "authorization: { scheme: Bearer, headerRef: { name: aq-api, key: token } }",
        "authorization: { header: x-api-key, scheme: Token, headerRef: { name: aq-api, key: key } }",
    );
    assert!(rendered(&source(&signed), "hsl-gbfs").contains("x-api-key: Token ${DS_HSL_GBFS_KEY}"));
}

const KAFKA: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: DataSource
metadata: { name: kafka-hel, namespace: bb-ovzdusie }
spec:
  type: kafka
  input:
    addresses: ["kafka.hel.fi:9093"]
    topics: ["sensors.air"]
    consumer_group: jc-helsinki-air
    tls: { enabled: true }
    sasl:
      mechanism: SCRAM-SHA-512
      user: hki-collector
      password: "${DS_KAFKA_HEL_PASSWORD}"
  secrets:
    - { name: kafka-hel, key: password, envVar: DS_KAFKA_HEL_PASSWORD }
"#;

const HTTP_CLIENT_RUNNER: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: DataSource
metadata: { name: http-runner, namespace: bb-ovzdusie }
spec:
  type: http_client
  input:
    url: https://example.com/feed
    verb: GET
"#;

#[test]
fn a_kafka_runner_source_renders_input_verbatim_with_no_prepended_processors() {
    let src = source(KAFKA);
    let out = render(AUTHORED, &src, &context("kafka-hel")).expect("renders");
    assert!(out.starts_with("input:"), "{out}");
    assert!(out.contains("kafka:"), "renders kafka input block:\n{out}");
    assert!(
        out.contains("${DS_KAFKA_HEL_PASSWORD}"),
        "interpolation untouched:\n{out}"
    );
    assert!(out.contains("kafka.hel.fi:9093"));

    let parsed: serde_norway::Value = serde_norway::from_str(&out).expect("parses");
    let processors = parsed
        .get("pipeline")
        .and_then(|p| p.get("processors"))
        .and_then(|p| p.as_sequence())
        .expect("processors");
    // Kafka source contributes NO prepended processor, so only the author's mapping is present.
    assert_eq!(
        processors.len(),
        1,
        "only author's mapping, no prepended decoder"
    );
    assert!(processors[0].get("mapping").is_some());
}

#[test]
fn an_http_client_runner_source_renders_verbatim_not_typed_shape() {
    let src = source(HTTP_CLIENT_RUNNER);
    let out = rendered(&src, "http-runner");
    assert!(out.starts_with("http_client:"), "{out}");
    assert!(out.contains("url: https://example.com/feed"));
    assert!(out.contains("verb: GET"));
    assert!(
        !out.contains("tls:"),
        "runner input does not synthesize a tls block"
    );
}

#[test]
fn a_scheduled_pipeline_with_a_non_terminating_source_is_refused() {
    let pipe: PipelineSpec = serde_norway::from_str(
        r#"class: scheduled
schedule: "*/15 * * * *"
targetEndpoint: urn:ngsi-ld:Endpoint:example.org:helsinki:ep-writer
"#,
    )
    .expect("valid pipeline spec");

    let src = source(KAFKA);
    let ctx = InputContext {
        source: "kafka-hel",
        project: "bb-ovzdusie",
        pipeline: "aq-kafka-ingest",
        pipeline_spec: Some(&pipe),
    };
    let err = render(AUTHORED, &src, &ctx).unwrap_err();
    assert!(
        matches!(err, RenderError::Class(_)),
        "expected RenderError::Class, got: {err:?}"
    );
}

const ZIP: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: DataSource
metadata: { name: hsl-gtfs, namespace: helsinki }
spec:
  type: http
  http:
    url: https://infopalvelut.storage.hsldev.com/gtfs/hsl.zip
    authorization: { header: x-key, headerRef: { name: hsl, key: key } }
    archive:
      format: zip
      members: [agency.txt, stops.txt]
      maxBytes: 1048576
"#;

/// PL-72: an archive is read by range: the input asks for the tail, the directory names the
/// members, and each member's own request carries the connection's credential and its range.
/// A refused read is logged and dropped before the author's processors see archive bytes.
#[test]
fn an_archive_reads_its_named_members_by_range_before_the_authors_processors() {
    let spec = source(ZIP);
    let out = render(
        "pipeline:\n  processors:\n    - unarchive: { format: csv }\n",
        &spec,
        &context("hsl-gtfs"),
    )
    .expect("renders");
    let doc: serde_norway::Value = serde_norway::from_str(&out).expect("yaml");
    let input = &doc["input"]["http_client"];
    assert_eq!(input["headers"]["Range"].as_str(), Some("bytes=-65557"));
    assert_eq!(
        input["headers"]["x-key"].as_str(),
        Some("${DS_HSL_GTFS_KEY}")
    );

    let names = |steps: &[serde_norway::Value]| -> Vec<String> {
        steps
            .iter()
            .map(|step| {
                step.as_mapping()
                    .unwrap()
                    .keys()
                    .next()
                    .unwrap()
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .collect()
    };
    let outer = doc["pipeline"]["processors"]
        .as_sequence()
        .expect("processors");
    assert_eq!(names(outer), ["try", "catch", "for_each", "unarchive"]);
    // The first refusal skips the rest, so an unchecked directory never reaches a request.
    let listing = outer[0]["try"].as_sequence().expect("the directory");
    assert_eq!(names(listing), ["mapping", "unarchive"]);
    // Each member is a batch of its own, so a refused one never stops another (T-4752).
    let each = outer[2]["for_each"].as_sequence().expect("per member");
    assert_eq!(names(each), ["try", "catch"]);
    let steps = each[0]["try"].as_sequence().expect("the read");
    assert_eq!(
        names(steps),
        ["mapping", "http", "mapping", "unarchive", "mapping"]
    );
    let directory = listing[0]["mapping"].as_str().unwrap();
    assert!(directory.contains(r#"let wanted = ["agency.txt","stops.txt"]"#));
    assert!(directory.contains("let max_bytes = 1048576"));
    assert!(directory.contains("e.size > $max_bytes || e.csize > $max_bytes"));
    assert!(!directory.contains("MEMBERS") && !directory.contains("MAX_BYTES"));
    let member = &steps[1]["http"];
    assert_eq!(member["url"], input["url"]);
    assert_eq!(member["headers"]["Range"].as_str(), Some("${! @range }"));
    assert_eq!(
        member["headers"]["x-key"].as_str(),
        Some("${DS_HSL_GTFS_KEY}")
    );
    // The directory may lie about sizes, so the inflate stops at the declared one: the local
    // step wraps the one member as an archive of its own, whose record declares the size, and
    // `unarchive: zip` refuses past it (T-4752; archive_read_bento_tests runs it).
    let local = steps[2]["mapping"].as_str().unwrap();
    assert!(
        local.contains(r#""504b0102" + $z.entry.slice(0, 8)"#),
        "{local}"
    );
    assert!(!local.contains("decompress"));
    assert_eq!(steps[3]["unarchive"]["format"].as_str(), Some("zip"));
    let check = steps[4]["mapping"].as_str().unwrap();
    assert!(check.contains("the directory says"));
    assert!(!out.contains("wasm"), "{out}");
    for refused in [&outer[1]["catch"], &each[1]["catch"]] {
        assert_eq!(refused[1]["mapping"].as_str(), Some("root = deleted()"));
    }
    assert_eq!(outer[3]["unarchive"]["format"].as_str(), Some("csv"));

    // Without `archive` the same connection renders as it did: no range, nothing prepended.
    let plain = source(
        &ZIP.split("    archive:")
            .next()
            .unwrap()
            .replace("hsl.zip", "x.json"),
    );
    let out = render("", &plain, &context("hsl-gtfs")).expect("renders");
    assert!(!out.contains("Range") && !out.contains("pipeline"), "{out}");
}
