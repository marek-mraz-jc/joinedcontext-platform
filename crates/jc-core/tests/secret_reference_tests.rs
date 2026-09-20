//! A secret reference names a secret, and never carries one (T-2238; MF-24, MF-35, CC-06, PL-16).
//!
//! `SecretRef` has `deny_unknown_fields`, so an inline `value:` cannot be parsed — but nothing
//! looked at the two fields it does have. A typed data source's `passwordRef.name` was never
//! validated at all: measured on dev, `{"name": "ghp_AAAA…"}` was checked **green**, the plan wrote
//! it into the manifest, and an approval would have committed a live GitHub token to the
//! configuration repository in the clear.
//!
//! Two rules, because either alone lets a credential through: a secret's name is a DNS-1123 label
//! (`ghp_…` is not), and a name carrying a credential's own shape is refused even when it is a
//! perfectly good label (`glpat-…`, `xoxb-…`, `sk-…` all are).
//!
//! Neither refusal repeats the value: the field is what the person is told, because the thing they
//! pasted is live and an error message is copied into chats and logs.
use jc_core::envelope::SecretRef;
use jc_core::error::Error;
use jc_core::kinds::data_source::DataSourceSpec;
use jc_core::names;

/// The credentials a person actually has in their clipboard, with the shapes that are also valid
/// DNS-1123 labels — the ones the label rule cannot catch.
const CREDENTIALS: &[(&str, &str)] = &[
    ("GitLab personal access token", "glpat-not-a-real-token"),
    // The bodies are deliberately short and obviously fake: a fixture that looks like a real
    // token is blocked by GitHub's own push protection, which is how this line got written.
    ("Slack bot token", "xoxb-not-a-real-token"),
    ("OpenAI key", "sk-not-a-real-key"),
    ("Docker Hub token", "dckr-pat-not-a-real-token"),
    ("npm token", "npm-not-a-real-token"),
    ("Hugging Face token", "hf-not-a-real-token"),
];

/// The names a project really uses, which must keep working: a refusal that breaks the seed is a
/// second defect and not a fix.
const REAL_NAMES: &[&str] = &[
    "aq-opendata",
    "helsinki-mqtt",
    "ckan-token",
    "s",
    "hsl-gtfs-rt-key",
];

fn mqtt_with_password_named(name: &str) -> DataSourceSpec {
    serde_json::from_value(serde_json::json!({
        "type": "mqtt",
        "mqtt": {
            "urls": ["tcp://mqtt.example.org:1883"],
            "topics": ["helsinki/#"],
            "passwordRef": { "name": name, "key": "password" },
        },
    }))
    .expect("a data source with a password reference parses")
}

/// MF-35: the name of a secret is a DNS-1123 label, on a typed connection as much as a runner one.
#[test]
fn a_typed_password_reference_names_a_label_and_not_a_token() {
    let refused = mqtt_with_password_named("ghp_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA")
        .validate()
        .expect_err("a token is not a secret name");

    let said = refused.to_string();
    assert!(
        said.contains("passwordRef"),
        "the refusal names the field a person typed into: {said}"
    );
    assert!(
        !said.contains("ghp_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"),
        "the refusal repeats the credential: {said}"
    );
}

/// MF-24: a name that is a credential is refused by its shape, although it is a valid label.
#[test]
fn a_secret_name_that_is_a_credential_is_refused_by_its_shape() {
    for (what, credential) in CREDENTIALS {
        assert!(
            names::validate_dns1123_label(credential).is_ok(),
            "{what} is not a valid label, so this case proves nothing: {credential}"
        );
        let refused = mqtt_with_password_named(credential)
            .validate()
            .unwrap_err_or_else_message(what);
        assert!(
            refused.contains("passwordRef"),
            "{what}: the refusal names no field: {refused}"
        );
        assert!(
            !refused.contains(credential),
            "{what}: the refusal repeats the credential: {refused}"
        );
    }
}

/// A key inside a secret is a Kubernetes secret key, and it is not a place for a value either.
#[test]
fn a_secret_key_of_the_wrong_shape_is_refused() {
    let spec: DataSourceSpec = serde_json::from_value(serde_json::json!({
        "type": "mqtt",
        "mqtt": {
            "urls": ["tcp://mqtt.example.org:1883"],
            "topics": ["helsinki/#"],
            "passwordRef": { "name": "helsinki-mqtt", "key": "glpat-not-a-real-token" },
        },
    }))
    .expect("parses");
    let refused = spec.validate().expect_err("a credential is not a key");
    assert!(refused.to_string().contains("passwordRef"), "{refused}");

    let spaced: DataSourceSpec = serde_json::from_value(serde_json::json!({
        "type": "mqtt",
        "mqtt": {
            "urls": ["tcp://mqtt.example.org:1883"],
            "topics": ["helsinki/#"],
            "passwordRef": { "name": "helsinki-mqtt", "key": "the password" },
        },
    }))
    .expect("parses");
    spaced
        .validate()
        .expect_err("a key with a space is no Kubernetes secret key");
}

/// The other references of the other types take the same walk, so the fix is not one field deep.
#[test]
fn every_typed_connection_validates_its_own_references() {
    let http: DataSourceSpec = serde_json::from_value(serde_json::json!({
        "type": "http",
        "http": {
            "url": "https://example.org/feed.json",
            "authorization": { "scheme": "Bearer", "headerRef": { "name": "glpat-not-a-real-token", "key": "token" } },
        },
    }))
    .expect("parses");
    let refused = http
        .validate()
        .expect_err("the header reference is checked");
    assert!(refused.to_string().contains("headerRef"), "{refused}");

    let tls: DataSourceSpec = serde_json::from_value(serde_json::json!({
        "type": "websocket",
        "webSocket": { "url": "wss://example.org/stream" },
        "tls": { "caCertRef": { "name": "Not-A-Label", "key": "ca.crt" } },
    }))
    .expect("parses");
    let refused = tls.validate().expect_err("the CA reference is checked");
    assert!(refused.to_string().contains("caCertRef"), "{refused}");
}

/// The names a project really uses still pass: the rule refuses credentials, not secrets.
#[test]
fn a_real_secret_reference_still_passes() {
    for name in REAL_NAMES {
        mqtt_with_password_named(name)
            .validate()
            .unwrap_or_else(|err| panic!("a real secret name was refused: {name}: {err}"));
    }
}

/// The shape rule on its own, for the callers that hold a name and no manifest.
#[test]
fn the_shape_rule_knows_a_credential_from_a_name() {
    for (what, credential) in CREDENTIALS {
        assert!(
            names::looks_like_a_credential(credential),
            "{what} is not recognised: {credential}"
        );
    }
    for name in REAL_NAMES {
        assert!(
            !names::looks_like_a_credential(name),
            "a real secret name is taken for a credential: {name}"
        );
    }
    // Case is not what makes a credential: a pasted token keeps its own.
    assert!(names::looks_like_a_credential("AKIAIOSFODNN7EXAMPLE"));
    assert!(names::looks_like_a_credential(
        "GHP_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
    ));
    assert!(names::looks_like_a_credential(
        "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.c2lnbmF0dXJl"
    ));
}

/// The error a secret reference raises never carries the value, whichever rule refused it.
#[test]
fn no_refusal_of_a_reference_repeats_what_it_refused() {
    for value in [
        "ghp_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        "glpat-not-a-real-token",
    ] {
        let refused = SecretRef {
            name: value.to_owned(),
            key: None,
            env_var: None,
        }
        .validate("spec.mqtt.passwordRef")
        .expect_err("refused");
        match &refused {
            Error::Invalid { field, reason } => {
                assert!(field.contains("passwordRef"), "{field}");
                assert!(!reason.contains(value), "the reason repeats it: {reason}");
            }
            other => panic!("a reference's refusal is Invalid, not {other:?}"),
        }
        assert!(!refused.to_string().contains(value), "{refused}");
    }
}

/// A convenience for reading the refusal as text in a loop.
trait RefusalText {
    fn unwrap_err_or_else_message(self, what: &str) -> String;
}

impl<T> RefusalText for Result<T, Error> {
    fn unwrap_err_or_else_message(self, what: &str) -> String {
        match self {
            Ok(_) => panic!("{what} was accepted as a secret name"),
            Err(err) => err.to_string(),
        }
    }
}
