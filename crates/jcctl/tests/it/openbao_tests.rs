//! Resolving `secretRef`s from OpenBao (T-0137, CC-06, OPS-37, PL-15, PL-17, ADR-N-012).
//!
//! The mock below is the whole server: it answers `auth/{mount}/login` and KV v2 reads the
//! way OpenBao documents them, records what it was asked, and can be told to fail. Because
//! the resolver opens no socket, every refusal it makes is reachable from here — including
//! the ones that matter most, a token that never expires and a value that reaches a log.

use jc_core::SecretRef;
use jcctl::secrets::openbao::{
    environment, login, service_account_jwt, BaoApi, BaoError, BaoStore, Settings,
};
use serde_json::{json, Value};
use std::collections::BTreeMap;

const ROLE: &str = "pipeline-runner-ovzdusie";
const PASSWORD: &str = "hunter2-and-then-some";

/// An OpenBao that answers from memory: one KV v2 tree, one login.
#[derive(Default)]
struct Bao {
    /// KV v2 read path -> the `data` object of the answer.
    paths: BTreeMap<String, Value>,
    /// What the login answers; `None` refuses it.
    auth: Option<Value>,
    /// Every path that was asked for, in order.
    calls: Vec<String>,
    /// The token the login handed out, so a test can look for it where it must not be.
    issued: String,
}

impl Bao {
    fn new() -> Self {
        Self {
            auth: Some(json!({
                "client_token": "hvs.CAESIJ-a-short-lived-one",
                "lease_duration": 3600,
                "renewable": true,
            })),
            issued: "hvs.CAESIJ-a-short-lived-one".to_owned(),
            ..Self::default()
        }
    }

    /// A live KV v2 secret at `secret/data/{prefix}/{name}`.
    fn with(mut self, path: &str, keys: &[(&str, Value)]) -> Self {
        let data: serde_json::Map<String, Value> = keys
            .iter()
            .map(|(k, v)| ((*k).to_owned(), v.clone()))
            .collect();
        self.paths.insert(
            path.to_owned(),
            json!({ "data": data, "metadata": { "version": 1, "destroyed": false } }),
        );
        self
    }

    /// A secret whose current version was deleted: 200, `data: null`, a deletion time.
    fn deleted(mut self, path: &str) -> Self {
        self.paths.insert(
            path.to_owned(),
            json!({ "data": null, "metadata": { "version": 2, "deletion_time": "2026-09-07T00:00:00Z" } }),
        );
        self
    }

    fn login_answer(mut self, auth: Value) -> Self {
        self.issued = auth
            .get("client_token")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        self.auth = Some(auth);
        self
    }
}

impl BaoApi for Bao {
    fn post(&mut self, path: &str, body: &Value) -> Result<Value, BaoError> {
        self.calls.push(path.to_owned());
        assert_eq!(
            body.get("role").and_then(Value::as_str),
            Some(ROLE),
            "the login names the role it is bound to"
        );
        assert!(
            body.get("jwt").and_then(Value::as_str).is_some(),
            "the login carries the ServiceAccount JWT"
        );
        match &self.auth {
            Some(auth) => Ok(json!({ "auth": auth })),
            None => Err(BaoError::Api {
                status: 403,
                path: path.to_owned(),
                message: "permission denied".to_owned(),
            }),
        }
    }

    fn get(&self, path: &str, token: &str) -> Result<Option<Value>, BaoError> {
        assert_eq!(token, self.issued, "the read carries the session token");
        // `self.calls` is not recorded here: `get` takes `&self`, which is the point —
        // reading a secret cannot change the store it reads from.
        Ok(self.paths.get(path).map(|data| json!({ "data": data })))
    }
}

fn settings() -> Settings {
    Settings::new(ROLE).under("projects/ovzdusie")
}

fn reference(name: &str, key: Option<&str>, env_var: Option<&str>) -> SecretRef {
    SecretRef {
        name: name.to_owned(),
        key: key.map(str::to_owned),
        env_var: env_var.map(str::to_owned),
    }
}

fn store(bao: &Bao, references: &[SecretRef]) -> Result<BaoStore, BaoError> {
    let mut session_source = Bao::new();
    session_source.auth.clone_from(&bao.auth);
    session_source.issued.clone_from(&bao.issued);
    let session = login(&mut session_source, &settings(), "a.b.c").expect("the login succeeds");
    BaoStore::fetch(bao, &settings(), &session, references)
}

#[test]
fn test_a_pipeline_secret_is_fetched_and_injected() {
    let bao = Bao::new().with(
        "secret/data/projects/ovzdusie/mqtt-credentials",
        &[("username", json!("mesto")), ("password", json!(PASSWORD))],
    );
    let references = vec![reference(
        "mqtt-credentials",
        Some("password"),
        Some("MQTT_PASSWORD"),
    )];

    let store = store(&bao, &references).expect("the secret is readable");
    assert_eq!(store.len(), 1);
    assert_eq!(store.names().collect::<Vec<_>>(), vec!["mqtt-credentials"]);

    let environment = environment(&references, &store).expect("the environment is complete");
    assert_eq!(environment.len(), 1);
    assert_eq!(
        environment["MQTT_PASSWORD"].expose(),
        PASSWORD,
        "the runner gets the value the reference names"
    );
}

#[test]
fn test_the_kv_v2_data_segment_is_in_the_path() {
    // The usual reason a correct policy reads nothing: KV v2 puts `data` between the mount
    // and the path, and v1 does not.
    let settings = settings();
    assert_eq!(
        settings.read_path("mqtt-credentials").unwrap(),
        "secret/data/projects/ovzdusie/mqtt-credentials"
    );
    assert_eq!(settings.login_path(), "auth/kubernetes/login");
    assert_eq!(
        Settings::new(ROLE).read_path("portal-session").unwrap(),
        "secret/data/portal-session",
        "a store with no per-project subtree reads at the mount root"
    );
    assert_eq!(
        Settings::new(ROLE)
            .mounted("k8s-dev", "jc")
            .under("/projects/doprava/")
            .read_path("gtfs-token")
            .unwrap(),
        "jc/data/projects/doprava/gtfs-token",
        "mounts and prefixes are trimmed, so a trailing slash does not double"
    );
}

#[test]
fn test_a_name_cannot_climb_out_of_its_project() {
    // The prefix is the tenancy boundary (ADR-N-012 §3): a name that is not one segment
    // would read another project's subtree with this project's grant.
    for name in ["../doprava/gtfs-token", "a/b", "", ".hidden"] {
        let refused = settings().read_path(name);
        assert!(
            matches!(refused, Err(BaoError::InvalidName { .. })),
            "{name:?} is not a single path segment"
        );
    }
}

#[test]
fn test_a_token_that_never_expires_is_refused() {
    // OPS-37: `auth/kubernetes` exists to hand out a token that dies with the apply. A zero
    // lease is Vault's "never expires", which is a root token in all but name.
    let mut bao = Bao::new().login_answer(json!({
        "client_token": "hvs.a-root-token",
        "lease_duration": 0,
        "renewable": false,
    }));
    let refused = login(&mut bao, &settings(), "a.b.c");
    assert!(
        matches!(refused, Err(BaoError::TokenNeverExpires { .. })),
        "expected the never-expiring token to be refused, got {refused:?}"
    );
}

#[test]
fn test_the_session_holds_the_lease_and_prints_none_of_the_token() {
    let mut bao = Bao::new();
    let session = login(&mut bao, &settings(), "a.b.c").expect("the login succeeds");
    assert_eq!(session.lease_seconds(), 3600);
    assert!(session.renewable());
    assert_eq!(bao.calls, vec!["auth/kubernetes/login".to_owned()]);

    let printed = format!("{session:?}");
    assert!(
        !printed.contains("hvs."),
        "PL-17: the session token must not reach a log line: {printed}"
    );
    assert!(printed.contains("3600"), "the lease is worth printing");
}

#[test]
fn test_no_error_message_carries_a_value() {
    // PL-17 is about logs, and an error message is the log line nobody edits. Every refusal
    // the resolver can make is walked here, and none of them may repeat a secret.
    let bao = Bao::new()
        .with(
            "secret/data/projects/ovzdusie/mqtt-credentials",
            &[("username", json!("mesto")), ("password", json!(PASSWORD))],
        )
        .with(
            "secret/data/projects/ovzdusie/numeric",
            &[("port", json!(1883))],
        )
        .deleted("secret/data/projects/ovzdusie/rotated");

    let live = vec![reference("mqtt-credentials", Some("password"), None)];
    let resolved = store(&bao, &live).expect("the secret is readable");

    let messages = vec![
        resolved
            .resolve(&reference("mqtt-credentials", Some("api-key"), None))
            .unwrap_err()
            .to_string(),
        resolved
            .resolve(&reference("mqtt-credentials", None, None))
            .unwrap_err()
            .to_string(),
        resolved
            .resolve(&reference("absent", None, None))
            .unwrap_err()
            .to_string(),
        store(&bao, &[reference("numeric", Some("port"), None)])
            .unwrap_err()
            .to_string(),
        store(&bao, &[reference("rotated", None, None)])
            .unwrap_err()
            .to_string(),
        store(&bao, &[reference("absent", None, None)])
            .unwrap_err()
            .to_string(),
        environment(&live, &resolved).unwrap_err().to_string(),
    ];
    for message in &messages {
        assert!(
            !message.contains(PASSWORD) && !message.contains("mesto"),
            "a refusal repeated a secret value: {message}"
        );
    }
}

#[test]
fn test_a_deleted_version_is_not_a_missing_secret() {
    // A rotated secret whose current version was deleted answers 200 with `data: null`. It
    // has to read differently from "never existed", or an operator looks in the wrong place.
    let bao = Bao::new().deleted("secret/data/projects/ovzdusie/rotated");
    let refused = store(&bao, &[reference("rotated", None, None)]);
    assert!(
        matches!(refused, Err(BaoError::SecretDeleted { .. })),
        "expected a deleted-version error, got {refused:?}"
    );

    let missing = store(&bao, &[reference("never-was", None, None)]);
    assert!(
        matches!(missing, Err(BaoError::SecretNotFound { .. })),
        "expected a not-found error, got {missing:?}"
    );
}

#[test]
fn test_a_reference_with_no_key_is_only_allowed_when_there_is_one() {
    let single = Bao::new().with(
        "secret/data/projects/ovzdusie/api-token",
        &[("token", json!("t-1"))],
    );
    let resolved = store(&single, &[reference("api-token", None, None)]).expect("readable");
    assert_eq!(
        resolved
            .resolve(&reference("api-token", None, None))
            .expect("one key needs no name")
            .expose(),
        "t-1"
    );

    let two = Bao::new().with(
        "secret/data/projects/ovzdusie/api-token",
        &[("token", json!("t-1")), ("refresh", json!("t-2"))],
    );
    let resolved = store(&two, &[reference("api-token", None, None)]).expect("readable");
    let refused = resolved.resolve(&reference("api-token", None, None));
    assert!(
        matches!(refused, Err(BaoError::KeyRequired { .. })),
        "a second key must turn an ambiguous reference into an error, got {refused:?}"
    );
}

#[test]
fn test_one_read_per_name_however_many_references() {
    // Two references into the same secret are one call, and a fetch that half succeeded is
    // no fetch at all: a runner started with half its environment fails on the credential
    // it needs, at the worst moment.
    let bao = Bao::new().with(
        "secret/data/projects/ovzdusie/mqtt-credentials",
        &[("username", json!("mesto")), ("password", json!(PASSWORD))],
    );
    let references = vec![
        reference("mqtt-credentials", Some("username"), Some("MQTT_USER")),
        reference("mqtt-credentials", Some("password"), Some("MQTT_PASSWORD")),
    ];
    let resolved = store(&bao, &references).expect("both are readable");
    assert_eq!(resolved.len(), 1, "one secret, read once");

    let environment = environment(&references, &resolved).expect("both are injected");
    assert_eq!(environment.len(), 2);
    assert_eq!(environment["MQTT_USER"].expose(), "mesto");

    let with_a_missing_one = vec![
        reference("mqtt-credentials", Some("password"), Some("MQTT_PASSWORD")),
        reference("absent", None, Some("NOPE")),
    ];
    assert!(
        store(&bao, &with_a_missing_one).is_err(),
        "one unreadable reference fails the whole fetch"
    );
}

#[test]
fn test_a_reference_without_an_env_var_is_refused() {
    // PL-16: `bento.yaml` interpolates `${MQTT_PASSWORD}` by name, so the manifest has to
    // say which name. Deriving one here would be a convention nothing else in the platform
    // shares, and the runner would look for a variable the author never wrote.
    let bao = Bao::new().with(
        "secret/data/projects/ovzdusie/mqtt-credentials",
        &[("password", json!(PASSWORD))],
    );
    let references = vec![reference("mqtt-credentials", Some("password"), None)];
    let resolved = store(&bao, &references).expect("readable");
    let refused = environment(&references, &resolved);
    assert!(
        matches!(refused, Err(BaoError::NoEnvVar { .. })),
        "expected a missing-envVar refusal, got {:?}",
        refused.map(|e| e.len())
    );
}

#[test]
fn test_a_malformed_answer_is_refused_rather_than_half_read() {
    struct Wrong;
    impl BaoApi for Wrong {
        fn post(&mut self, _path: &str, _body: &Value) -> Result<Value, BaoError> {
            Ok(json!({ "auth": { "client_token": "", "lease_duration": 3600 } }))
        }
        fn get(&self, _path: &str, _token: &str) -> Result<Option<Value>, BaoError> {
            unreachable!("the login fails first")
        }
    }
    let refused = login(&mut Wrong, &settings(), "a.b.c");
    assert!(
        matches!(refused, Err(BaoError::Malformed { .. })),
        "an empty client_token is not a session, got {refused:?}"
    );

    struct NoData;
    impl BaoApi for NoData {
        fn post(&mut self, _path: &str, _body: &Value) -> Result<Value, BaoError> {
            unreachable!("the session is made elsewhere")
        }
        fn get(&self, _path: &str, _token: &str) -> Result<Option<Value>, BaoError> {
            // KV v1 shape: the values are directly under `data`, with no second level.
            Ok(Some(json!({ "data": "not-an-object" })))
        }
    }
    let mut bao = Bao::new();
    let session = login(&mut bao, &settings(), "a.b.c").expect("the login succeeds");
    let refused = BaoStore::fetch(
        &NoData,
        &settings(),
        &session,
        &[reference("mqtt-credentials", None, None)],
    );
    assert!(
        matches!(refused, Err(BaoError::Malformed { .. })),
        "a KV v1 answer is not a KV v2 answer, got {refused:?}"
    );
}

#[test]
fn test_the_service_account_token_is_read_and_an_empty_one_is_refused() {
    let dir = std::env::temp_dir().join(format!("jcctl-openbao-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("scratch directory");

    let good = dir.join("token");
    std::fs::write(&good, "header.payload.signature\n").expect("write");
    assert_eq!(
        service_account_jwt(&good).expect("readable").as_str(),
        "header.payload.signature",
        "the trailing newline of a projected token is not part of it"
    );

    let empty = dir.join("empty");
    std::fs::write(&empty, "   \n").expect("write");
    assert!(
        matches!(
            service_account_jwt(&empty),
            Err(BaoError::EmptyServiceAccountToken { .. })
        ),
        "a pod with no projected token cannot log in"
    );

    let missing = service_account_jwt(&dir.join("absent"));
    assert!(matches!(missing, Err(BaoError::Io { .. })));

    std::fs::remove_dir_all(&dir).ok();
}
