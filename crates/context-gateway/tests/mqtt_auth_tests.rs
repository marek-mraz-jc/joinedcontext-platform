//! The MQTT edge brokers' CONNECT check at `/internal/mqtt/auth` (T-3837, PL-75, PL-76,
//! ADR-N-048). One test per row of the ADR's security analysis that the gateway holds: a camera
//! publishes under its own client id only, a token for another broker or another project, an
//! expired one and one under another client id are refused, a broker asks with its own mesh
//! identity only, and the runner connects without a token from inside the cluster alone.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use context_gateway::app::{router, Gateway};
use context_gateway::auth::accounts::accounts_of;
use context_gateway::mqtt::{brokers_of, decide, internal_router, Brokers, Connect, Pods};
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tower::ServiceExt;

const CAMERAS: &str = "mqtt-ovzdusie-cameras.jc-mqtt.serviceaccount.identity.linkerd.cluster.local";
const METERS: &str = "mqtt-ovzdusie-meters.jc-mqtt.serviceaccount.identity.linkerd.cluster.local";
const PROBE: &str = "mqtt-ovzdusie-probe.jc-mqtt.serviceaccount.identity.linkerd.cluster.local";
const PLAIN: &str = "mqtt-ovzdusie-plain.jc-mqtt.serviceaccount.identity.linkerd.cluster.local";
const ESPOO: &str = "mqtt-espoo-cameras.jc-mqtt.serviceaccount.identity.linkerd.cluster.local";

fn write(dir: &Path, rel: &str, body: &str) {
    let path = dir.join(rel);
    std::fs::create_dir_all(path.parent().expect("a parent")).expect("a directory");
    std::fs::write(path, body).expect("written");
}

fn account(dir: &Path, project: &str, name: &str) {
    write(
        dir,
        &format!("projects/{project}/access/serviceaccounts/{name}.yaml"),
        &format!("apiVersion: joinedcontext.com/v1alpha1\nkind: ServiceAccount\nmetadata:\n  name: {name}\n  namespace: {project}\nspec:\n  owner:\n    user: demo.steward\n  purpose: \"a camera\"\n  roles:\n    - role: space-writer\n      scope:\n        project: {project}\n  credentials:\n    - kind: oauth-client\n      name: default\n"),
    );
}

/// Two relay brokers of `ovzdusie` (cameras, meters), a capture broker (probe) and an ingest one
/// (plain), a camera of `espoo` holding a role on the cameras broker by name, and a DataSource
/// reading the cameras broker.
fn repository(test: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("gateway-mqtt-{test}"));
    let _ = std::fs::remove_dir_all(&dir);
    account(&dir, "ovzdusie", "cam-north");
    account(&dir, "ovzdusie", "cam-idle");
    account(&dir, "espoo", "cam-north");
    for (name, accounts) in [("cameras", "[cam-north]"), ("meters", "[cam-idle]")] {
        write(
            &dir,
            &format!("projects/ovzdusie/mqttbrokers/{name}.yaml"),
            &format!("apiVersion: joinedcontext.com/v1alpha1\nkind: MqttBroker\nmetadata:\n  name: {name}\n  namespace: ovzdusie\nspec:\n  mode: relay\n  roles:\n    - name: camera\n      serviceAccounts: {accounts}\n      publish: [\"cameras/${{clientid}}/#\"]\n      subscribe: [\"commands/${{clientid}}/+\"]\n"),
        );
    }
    for (name, mode) in [("probe", "capture"), ("plain", "ingest")] {
        write(
            &dir,
            &format!("projects/ovzdusie/mqttbrokers/{name}.yaml"),
            &format!("apiVersion: joinedcontext.com/v1alpha1\nkind: MqttBroker\nmetadata:\n  name: {name}\n  namespace: ovzdusie\nspec:\n  mode: {mode}\n  roles:\n    - name: sensor\n      serviceAccounts: [cam-north]\n      sharedTopics: true\n      publish: [\"air/${{clientid}}/pm\", \"air/status\"]\n"),
        );
    }
    write(
        &dir,
        "projects/espoo/mqttbrokers/cameras.yaml",
        "apiVersion: joinedcontext.com/v1alpha1\nkind: MqttBroker\nmetadata:\n  name: cameras\n  namespace: espoo\nspec:\n  roles:\n    - name: camera\n      serviceAccounts: [cam-north]\n      publish: [\"cameras/${clientid}/#\"]\n",
    );
    write(
        &dir,
        "projects/espoo/datasources/frames.yaml",
        "apiVersion: joinedcontext.com/v1alpha1\nkind: DataSource\nmetadata:\n  name: frames\n  namespace: espoo\nspec:\n  type: mqtt\n  mqtt:\n    brokerRef: { kind: MqttBroker, name: cameras }\n    topics: [\"cameras/+/frame\"]\n",
    );
    write(
        &dir,
        "projects/ovzdusie/datasources/frames.yaml",
        "apiVersion: joinedcontext.com/v1alpha1\nkind: DataSource\nmetadata:\n  name: frames\n  namespace: ovzdusie\nspec:\n  type: mqtt\n  mqtt:\n    brokerRef: { kind: MqttBroker, name: cameras }\n    topics: [\"cameras/+/frame\", \"cameras/+/status\"]\n",
    );
    dir
}

struct Fixture {
    realm: common::Realm,
    brokers: Brokers,
    accounts: context_gateway::auth::accounts::ServiceAccounts,
    dir: PathBuf,
}

impl Fixture {
    fn new(test: &str) -> Self {
        let dir = repository(test);
        let repo = jc_manifest::loader::Repository::load(&dir).expect("the repository loads");
        Self {
            realm: common::Realm::new(),
            brokers: brokers_of(&repo),
            accounts: accounts_of(&repo),
            dir,
        }
    }

    fn token(&self, azp: &str, audience: &str, exp_in: i64) -> String {
        self.realm.mint(&json!({
            "iss": common::ISSUER,
            "sub": format!("service-account-{azp}"),
            "aud": audience,
            "azp": azp,
            "exp": common::in_seconds(exp_in),
            "iat": common::in_seconds(-10),
        }))
    }

    fn decide(&self, caller: Option<&str>, connect: &Connect) -> Value {
        self.decide_why(caller, connect).0
    }

    /// The answer and the reason code a refusal is recorded with (T-5534).
    fn decide_why(&self, caller: Option<&str>, connect: &Connect) -> (Value, Option<&'static str>) {
        let verifier = self.realm.verifier();
        let (answer, why) = decide(
            &self.brokers,
            Some(&verifier),
            &self.accounts,
            caller,
            &Pods {
                runner: RUNNER_PODS
                    .map(|ip| ip.parse().expect("an address"))
                    .to_vec(),
                portal: vec![PORTAL_POD.parse().expect("an address")],
            },
            connect,
        );
        (
            serde_json::to_value(answer).expect("serializes"),
            why.refused.map(|reason| reason.code()),
        )
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn device(azp: &str, token: String) -> Connect {
    Connect {
        clientid: azp.to_owned(),
        username: azp.to_owned(),
        password: token,
        ipaddr: "203.0.113.7".to_owned(),
    }
}

/// The addresses the runner's headless Service resolves to in these tests.
const RUNNER_PODS: [&str; 2] = ["10.42.0.17", "fd00::17"];

/// The address the Portal's headless Service resolves to in these tests.
const PORTAL_POD: &str = "10.42.0.30";

fn capture(ipaddr: &str) -> Connect {
    Connect {
        clientid: "jc-portal-capture-mqtt-ovzdusie-probe".to_owned(),
        username: "jc-portal-capture".to_owned(),
        password: String::new(),
        ipaddr: ipaddr.to_owned(),
    }
}

fn runner(ipaddr: &str) -> Connect {
    Connect {
        clientid: "jc-ovzdusie-frames".to_owned(),
        username: "pipeline-runner-ovzdusie".to_owned(),
        password: String::new(),
        ipaddr: ipaddr.to_owned(),
    }
}

/// PL-75: a camera's token for this broker gets its role's filters, bound to its client id by
/// the broker, until the token expires.
#[test]
fn a_camera_with_a_token_for_this_broker_gets_its_roles_topics_until_exp() {
    let f = Fixture::new("allow");
    let token = f.token("ovzdusie-cam-north", "mqtt-ovzdusie-cameras", 300);
    let answer = f.decide(Some(CAMERAS), &device("ovzdusie-cam-north", token));
    assert_eq!(answer["result"], "allow", "{answer}");
    assert_eq!(answer["superuser"], false);
    assert!(answer["expire_at"].as_i64().expect("an expiry") > common::in_seconds(200));
    assert_eq!(
        answer["acl"],
        json!([
            {"permission": "allow", "action": "publish", "topic": "cameras/${clientid}/#", "qos": [0, 1], "retain": false},
            {"permission": "allow", "action": "subscribe", "topic": "commands/${clientid}/+", "qos": [0, 1]}
        ])
    );
}

/// The refusals of the ADR's security analysis, each with the rest of the CONNECT valid.
#[test]
fn every_way_a_camera_is_refused() {
    let f = Fixture::new("refused");
    let good = |f: &Fixture| f.token("ovzdusie-cam-north", "mqtt-ovzdusie-cameras", 300);
    let cases: Vec<(&str, Option<&str>, Connect, Option<&str>)> = vec![
        (
            "a token for another broker",
            Some(CAMERAS),
            device(
                "ovzdusie-cam-north",
                f.token("ovzdusie-cam-north", "mqtt-ovzdusie-meters", 300),
            ),
            Some("wrong_audience"),
        ),
        (
            "an expired token",
            Some(CAMERAS),
            device(
                "ovzdusie-cam-north",
                f.token("ovzdusie-cam-north", "mqtt-ovzdusie-cameras", -120),
            ),
            Some("expired"),
        ),
        (
            "another client id than the token's",
            Some(CAMERAS),
            Connect {
                clientid: "ovzdusie-cam-south".to_owned(),
                ..device("ovzdusie-cam-north", good(&f))
            },
            Some("wrong_client"),
        ),
        (
            "another username than the token's",
            Some(CAMERAS),
            Connect {
                username: "ovzdusie-cam-south".to_owned(),
                ..device("ovzdusie-cam-north", good(&f))
            },
            Some("wrong_client"),
        ),
        (
            "an account of another project, same name",
            Some(CAMERAS),
            device(
                "espoo-cam-north",
                f.token("espoo-cam-north", "mqtt-ovzdusie-cameras", 300),
            ),
            Some("other_project"),
        ),
        (
            "an account with no role on this broker",
            Some(CAMERAS),
            device(
                "ovzdusie-cam-idle",
                f.token("ovzdusie-cam-idle", "mqtt-ovzdusie-cameras", 300),
            ),
            Some("no_role"),
        ),
        (
            "a token signed by nobody the realm publishes",
            Some(CAMERAS),
            device(
                "ovzdusie-cam-north",
                f.realm.mint_with_kid(
                    "forged",
                    &json!({"iss": common::ISSUER, "sub": "x", "aud": "mqtt-ovzdusie-cameras", "azp": "ovzdusie-cam-north", "exp": common::in_seconds(300)}),
                ),
            ),
            Some("invalid_token"),
        ),
        (
            "no mesh identity",
            None,
            device("ovzdusie-cam-north", good(&f)),
            None,
        ),
        (
            "a broker asking with another broker's token audience",
            Some(METERS),
            device("ovzdusie-cam-north", good(&f)),
            Some("wrong_audience"),
        ),
        (
            "a caller that is no broker",
            Some("context-gateway.jc.serviceaccount.identity.linkerd.cluster.local"),
            device("ovzdusie-cam-north", good(&f)),
            None,
        ),
    ];
    for (why, caller, connect, reason) in cases {
        let (answer, recorded) = f.decide_why(caller, &connect);
        assert_eq!(
            answer,
            json!({"result": "deny", "superuser": false}),
            "{why}"
        );
        assert_eq!(recorded, reason, "{why}");
    }
}

/// PL-76, T-4004: the runner connects without a token from one of its own pods only, and may
/// only subscribe to what the DataSources naming this broker read.
#[test]
fn the_runner_subscribes_to_its_data_sources_topics_from_its_own_pods_only() {
    let f = Fixture::new("runner");
    for inside in ["10.42.0.17", "fd00::17", "::ffff:10.42.0.17"] {
        let answer = f.decide(Some(CAMERAS), &runner(inside));
        assert_eq!(answer["result"], "allow", "{inside}: {answer}");
        assert!(answer.get("expire_at").is_none(), "{answer}");
        assert_eq!(
            answer["acl"],
            json!([
                {"permission": "allow", "action": "subscribe", "topic": "cameras/+/frame", "qos": [0, 1]},
                {"permission": "allow", "action": "subscribe", "topic": "cameras/+/status", "qos": [0, 1]}
            ])
        );
    }
    let deny = json!({"result": "deny", "superuser": false});
    // Through Traefik the PROXY protocol carries the public address, or a cluster pod's own:
    // a private address that is no runner pod's is refused like a public one.
    for elsewhere in ["203.0.113.7", "10.42.0.99", "127.0.0.1", "::1", "fd00::99"] {
        assert_eq!(
            f.decide(Some(CAMERAS), &runner(elsewhere)),
            deny,
            "{elsewhere}"
        );
    }
    assert_eq!(f.decide(Some(CAMERAS), &runner("not an address")), deny);
    // Without a token only the runner's name passes, and only on a broker a DataSource reads.
    let someone = Connect {
        username: "ovzdusie-cam-north".to_owned(),
        ..runner("10.42.0.17")
    };
    assert_eq!(f.decide(Some(CAMERAS), &someone), deny);
    assert_eq!(f.decide(Some(METERS), &runner("10.42.0.17")), deny);
}

/// T-5561: the runner signs in under its project's name, and a broker grants only its own
/// project's runner: a stream of espoo that dials ovzdusie's broker from a runner pod, under
/// espoo's name or the bare one, gets no topic.
#[test]
fn the_runner_of_another_project_gets_no_topic() {
    let f = Fixture::new("runner-project");
    let deny = json!({"result": "deny", "superuser": false});
    let espoo = Connect {
        username: "pipeline-runner-espoo".to_owned(),
        ..runner("10.42.0.17")
    };
    let answer = f.decide(Some(ESPOO), &espoo);
    assert_eq!(answer["result"], "allow", "{answer}");
    assert_eq!(
        answer["acl"],
        json!([{"permission": "allow", "action": "subscribe", "topic": "cameras/+/frame", "qos": [0, 1]}])
    );
    assert_eq!(f.decide(Some(CAMERAS), &espoo), deny);
    assert_eq!(f.decide(Some(ESPOO), &runner("10.42.0.17")), deny);
    let bare = Connect {
        username: "pipeline-runner".to_owned(),
        ..runner("10.42.0.17")
    };
    assert_eq!(f.decide(Some(CAMERAS), &bare), deny);
    assert_eq!(f.decide(Some(ESPOO), &bare), deny);
}

/// ADR-N-048 §2.10: the Portal's capture subscriber connects without a token from a Portal pod,
/// on a capture broker only, and may subscribe to the publish filters of its roles, each
/// `${clientid}` level widened; it never publishes and never reads an ingest or relay broker.
#[test]
fn the_portal_subscribes_to_a_capture_brokers_publish_filters_only() {
    let f = Fixture::new("capture");
    let answer = f.decide(Some(PROBE), &capture(PORTAL_POD));
    assert_eq!(answer["result"], "allow", "{answer}");
    assert!(answer.get("expire_at").is_none(), "{answer}");
    assert_eq!(
        answer["acl"],
        json!([
            {"permission": "allow", "action": "subscribe", "topic": "air/+/pm", "qos": [0, 1]},
            {"permission": "allow", "action": "subscribe", "topic": "air/status", "qos": [0, 1]}
        ])
    );
    let deny = json!({"result": "deny", "superuser": false});
    for broker in [PLAIN, CAMERAS, METERS] {
        assert_eq!(
            f.decide(Some(broker), &capture(PORTAL_POD)),
            deny,
            "{broker}"
        );
    }
    // Only from a Portal pod: a runner pod or any other address is refused under that name,
    // and the Portal's pods do not pass as the runner.
    for elsewhere in [
        RUNNER_PODS[0],
        "10.42.0.99",
        "203.0.113.7",
        "not an address",
    ] {
        assert_eq!(
            f.decide(Some(PROBE), &capture(elsewhere)),
            deny,
            "{elsewhere}"
        );
    }
    assert_eq!(f.decide(Some(CAMERAS), &runner(PORTAL_POD)), deny);
}

/// The route answers on the internal listener's router only, always 200, and never on the
/// public surface APISIX fronts.
#[tokio::test]
async fn the_check_answers_on_the_internal_router_and_nowhere_else() {
    let f = Fixture::new("router");
    let gateway = Gateway::new(
        Broker::new("http://broker.invalid:1026"),
        Box::new(PolicyPdp),
        "banskabystrica.sk",
    )
    .authenticate(Arc::new(f.realm.verifier()), f.accounts.clone(), None);
    gateway.replace_brokers(f.brokers.clone());
    let gateway = Arc::new(gateway);
    let body = json!({
        "clientid": "ovzdusie-cam-north",
        "username": "ovzdusie-cam-north",
        "password": f.token("ovzdusie-cam-north", "mqtt-ovzdusie-cameras", 300),
        "ipaddr": "203.0.113.7"
    })
    .to_string();
    let request = |with_identity: bool| {
        let mut builder =
            Request::post("/internal/mqtt/auth").header("content-type", "application/json");
        if with_identity {
            builder = builder.header("l5d-client-id", CAMERAS);
        }
        builder.body(Body::from(body.clone())).expect("a request")
    };

    for (with_identity, result) in [(true, "allow"), (false, "deny")] {
        let response = internal_router(Arc::clone(&gateway), None, None, None)
            .oneshot(request(with_identity))
            .await
            .expect("answers");
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .expect("a body");
        let answer: Value = serde_json::from_slice(&bytes).expect("json");
        assert_eq!(answer["result"], result, "{answer}");
    }

    let public = router(Arc::clone(&gateway))
        .oneshot(request(true))
        .await
        .expect("answers");
    assert_ne!(
        public.status(),
        StatusCode::OK,
        "the public surface serves no CONNECT check"
    );
}

/// T-4004: on the internal router, the tokenless runner is admitted from what the configured
/// runner host resolves to, and from nowhere when no host is configured.
#[tokio::test]
async fn the_router_admits_the_runner_from_its_hosts_addresses_only() {
    let f = Fixture::new("runner-router");
    let gateway = Gateway::new(
        Broker::new("http://broker.invalid:1026"),
        Box::new(PolicyPdp),
        "banskabystrica.sk",
    )
    .authenticate(Arc::new(f.realm.verifier()), f.accounts.clone(), None);
    gateway.replace_brokers(f.brokers.clone());
    let gateway = Arc::new(gateway);
    let ask = |host: Option<&str>, ipaddr: &str| {
        let app = internal_router(Arc::clone(&gateway), host.map(str::to_owned), None, None);
        let body = json!({
            "clientid": "jc-ovzdusie-frames", "username": "pipeline-runner-ovzdusie",
            "password": "", "ipaddr": ipaddr
        })
        .to_string();
        async move {
            let request = Request::post("/internal/mqtt/auth")
                .header("content-type", "application/json")
                .header("l5d-client-id", CAMERAS)
                .body(Body::from(body))
                .expect("a request");
            let response = app.oneshot(request).await.expect("answers");
            let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
                .await
                .expect("a body");
            serde_json::from_slice::<Value>(&bytes).expect("json")["result"].clone()
        }
    };
    assert_eq!(ask(Some("localhost"), "127.0.0.1").await, "allow");
    assert_eq!(ask(Some("localhost"), "10.42.0.17").await, "deny");
    assert_eq!(ask(None, "127.0.0.1").await, "deny");
    assert_eq!(
        ask(Some("no-such-runner.invalid"), "127.0.0.1").await,
        "deny"
    );
}

/// T-5534: a refused CONNECT is kept for its broker with its reason and the broker's own account,
/// never the token or the client id, and only the Portal's mesh identity reads it.
#[tokio::test]
async fn a_refusal_is_kept_without_its_token_and_read_by_the_portal_alone() {
    let f = Fixture::new("refusals");
    let gateway = Gateway::new(
        Broker::new("http://broker.invalid:1026"),
        Box::new(PolicyPdp),
        "banskabystrica.sk",
    )
    .authenticate(Arc::new(f.realm.verifier()), f.accounts.clone(), None);
    gateway.replace_brokers(f.brokers.clone());
    let app = internal_router(
        Arc::new(gateway),
        None,
        None,
        Some("portal.portal".to_owned()),
    );
    let expired = f.token("ovzdusie-cam-north", "mqtt-ovzdusie-cameras", -120);
    let connect = json!({
        "clientid": "ovzdusie-cam-north-7f3a", "username": "ovzdusie-cam-north",
        "password": expired, "ipaddr": "203.0.113.7"
    })
    .to_string();
    let refused = app
        .clone()
        .oneshot(
            Request::post("/internal/mqtt/auth")
                .header("content-type", "application/json")
                .header("l5d-client-id", CAMERAS)
                .body(Body::from(connect))
                .expect("a request"),
        )
        .await
        .expect("answers");
    assert_eq!(refused.status(), StatusCode::OK);

    let read = |identity: Option<&str>| {
        let mut builder = Request::get("/internal/mqtt/refusals/mqtt-ovzdusie-cameras");
        if let Some(identity) = identity {
            builder = builder.header("l5d-client-id", identity);
        }
        app.clone()
            .oneshot(builder.body(Body::empty()).expect("a request"))
    };
    let answer = read(Some(
        "portal.portal.serviceaccount.identity.linkerd.cluster.local",
    ))
    .await
    .expect("answers");
    assert_eq!(answer.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(answer.into_body(), 1 << 20)
        .await
        .expect("a body");
    let text = String::from_utf8(bytes.to_vec()).expect("text");
    let kept: Value = serde_json::from_str(&text).expect("json");
    assert_eq!(kept["refusals"].as_array().map(Vec::len), Some(1), "{kept}");
    assert_eq!(kept["refusals"][0]["reason"], "expired");
    assert_eq!(
        kept["refusals"][0]["account"],
        "ovzdusie-cam-north".replace("ovzdusie-", "")
    );
    for secret in [expired.as_str(), "ovzdusie-cam-north-7f3a", "203.0.113.7"] {
        assert!(!text.contains(secret), "{text}");
    }

    for other in [Some(CAMERAS), None] {
        let answer = read(other).await.expect("answers");
        assert_eq!(answer.status(), StatusCode::FORBIDDEN, "{other:?}");
    }
}
