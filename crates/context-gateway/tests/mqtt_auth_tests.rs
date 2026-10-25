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
use context_gateway::mqtt::{
    brokers_of, decide, internal_router, publish_check, Access, Brokers, Connect, Pods,
    PUBLISH_CACHE_MS,
};
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

fn write(dir: &Path, rel: &str, body: &str) {
    let path = dir.join(rel);
    std::fs::create_dir_all(path.parent().expect("a parent")).expect("a directory");
    std::fs::write(path, body).expect("written");
}

fn account(dir: &Path, project: &str, name: &str) {
    account_until(dir, project, name, "");
}

/// An account whose one credential carries `expires` (an `expiresAt` line, or nothing).
fn account_until(dir: &Path, project: &str, name: &str, expires: &str) {
    write(
        dir,
        &format!("projects/{project}/access/serviceaccounts/{name}.yaml"),
        &format!("apiVersion: joinedcontext.com/v1alpha1\nkind: ServiceAccount\nmetadata:\n  name: {name}\n  namespace: {project}\nspec:\n  owner:\n    user: demo.steward\n  purpose: \"a camera\"\n  roles:\n    - role: space-writer\n      scope:\n        project: {project}\n  credentials:\n    - kind: oauth-client\n      name: default\n{expires}"),
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
    let soon = chrono::Utc::now() + chrono::Duration::seconds(100);
    let gone = chrono::Utc::now() - chrono::Duration::seconds(5);
    for (name, end) in [("cam-soon", soon), ("cam-gone", gone)] {
        account_until(
            &dir,
            "ovzdusie",
            name,
            &format!("      expiresAt: \"{}\"\n", end.to_rfc3339()),
        );
    }
    for (name, accounts) in [
        ("cameras", "[cam-north, cam-soon, cam-gone]"),
        ("meters", "[cam-idle]"),
    ] {
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
            &format!("apiVersion: joinedcontext.com/v1alpha1\nkind: MqttBroker\nmetadata:\n  name: {name}\n  namespace: ovzdusie\nspec:\n  mode: {mode}\n  roles:\n    - name: sensor\n      serviceAccounts: [cam-north]\n      publish: [\"air/${{clientid}}/pm\", \"air/status\"]\n"),
        );
    }
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
        let verifier = self.realm.verifier();
        let (answer, _) = decide(
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
        serde_json::to_value(answer).expect("serializes")
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
        username: "pipeline-runner".to_owned(),
        password: String::new(),
        ipaddr: ipaddr.to_owned(),
    }
}

/// PL-75: a camera's token for this broker gets its role's subscriptions, bound to its client id
/// by the broker, until the token expires; its publishes are checked one by one (T-5560).
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
            {"permission": "allow", "action": "subscribe", "topic": "commands/${clientid}/+", "qos": [0, 1]}
        ])
    );
}

/// T-5560: the verifier's leeway never reaches the broker: a token one second past `exp` is
/// refused at CONNECT rather than admitted with an `expire_at` already gone.
#[test]
fn a_token_past_exp_is_refused_without_leeway() {
    let f = Fixture::new("leeway");
    let deny = json!({"result": "deny", "superuser": false});
    for past in [-1, -30, -59] {
        let token = f.token("ovzdusie-cam-north", "mqtt-ovzdusie-cameras", past);
        assert_eq!(
            f.decide(Some(CAMERAS), &device("ovzdusie-cam-north", token)),
            deny,
            "{past} s"
        );
    }
}

/// T-5560: a credential's `expiresAt` ends the session even when the token lives longer, and an
/// account whose credentials have all ended neither connects nor publishes.
#[test]
fn a_credential_expiry_caps_the_session() {
    let f = Fixture::new("credential");
    let token = f.token("ovzdusie-cam-soon", "mqtt-ovzdusie-cameras", 3600);
    let answer = f.decide(Some(CAMERAS), &device("ovzdusie-cam-soon", token));
    assert_eq!(answer["result"], "allow", "{answer}");
    let expire_at = answer["expire_at"].as_i64().expect("an expiry");
    assert!(
        (common::in_seconds(90)..=common::in_seconds(100)).contains(&expire_at),
        "{answer}"
    );
    let token = f.token("ovzdusie-cam-gone", "mqtt-ovzdusie-cameras", 3600);
    assert_eq!(
        f.decide(Some(CAMERAS), &device("ovzdusie-cam-gone", token)),
        json!({"result": "deny", "superuser": false})
    );
    assert_eq!(
        publish(
            &f.brokers,
            &f.accounts,
            "ovzdusie-cam-gone",
            "cameras/ovzdusie-cam-gone/frame"
        ),
        (false, "the account's credentials have expired")
    );
}

fn publish(
    brokers: &Brokers,
    accounts: &context_gateway::auth::accounts::ServiceAccounts,
    clientid: &str,
    topic: &str,
) -> (bool, &'static str) {
    publish_check(
        brokers,
        accounts,
        Some(CAMERAS),
        &Access {
            access: "2".into(),
            clientid: clientid.into(),
            username: clientid.into(),
            topic: topic.into(),
        },
    )
}

/// T-5560: each publish is decided against the repository as it is now, so a camera whose role
/// or account is removed stops publishing at its next check, not at its token's `exp`.
#[test]
fn a_publish_is_decided_by_the_repository_as_it_is_now() {
    let f = Fixture::new("publish");
    let own = "cameras/ovzdusie-cam-north/frame";
    assert_eq!(
        publish(&f.brokers, &f.accounts, "ovzdusie-cam-north", own),
        (true, "a device's role")
    );
    assert_eq!(
        publish(
            &f.brokers,
            &f.accounts,
            "ovzdusie-cam-north",
            "cameras/ovzdusie-cam-soon/frame"
        ),
        (false, "no role of the account covers the topic")
    );
    // Another broker's caller, a subscription, a tokenless name, a username that is not the
    // client id and an account of another project are refused whatever the topic.
    let check = |caller, access: &str, clientid: &str, username: &str| {
        publish_check(
            &f.brokers,
            &f.accounts,
            caller,
            &Access {
                access: access.into(),
                clientid: clientid.into(),
                username: username.into(),
                topic: format!("cameras/{clientid}/frame"),
            },
        )
        .0
    };
    assert!(check(
        Some(CAMERAS),
        "2",
        "ovzdusie-cam-north",
        "ovzdusie-cam-north"
    ));
    assert!(!check(
        Some(METERS),
        "2",
        "ovzdusie-cam-north",
        "ovzdusie-cam-north"
    ));
    assert!(!check(
        None,
        "2",
        "ovzdusie-cam-north",
        "ovzdusie-cam-north"
    ));
    assert!(!check(
        Some(CAMERAS),
        "1",
        "ovzdusie-cam-north",
        "ovzdusie-cam-north"
    ));
    assert!(!check(
        Some(CAMERAS),
        "2",
        "pipeline-runner",
        "pipeline-runner"
    ));
    assert!(!check(
        Some(CAMERAS),
        "2",
        "ovzdusie-cam-north",
        "pipeline-runner"
    ));
    assert!(!check(
        Some(CAMERAS),
        "2",
        "espoo-cam-north",
        "espoo-cam-north"
    ));

    // The role taken away, then the account deleted: the same publish is refused.
    write(
        &f.dir,
        "projects/ovzdusie/mqttbrokers/cameras.yaml",
        "apiVersion: joinedcontext.com/v1alpha1\nkind: MqttBroker\nmetadata:\n  name: cameras\n  namespace: ovzdusie\nspec:\n  mode: relay\n  roles:\n    - name: camera\n      serviceAccounts: [cam-soon]\n      publish: [\"cameras/${clientid}/#\"]\n",
    );
    let repo = jc_manifest::loader::Repository::load(&f.dir).expect("the repository loads");
    assert_eq!(
        publish(
            &brokers_of(&repo),
            &accounts_of(&repo),
            "ovzdusie-cam-north",
            own
        ),
        (false, "no role of the account covers the topic")
    );
    std::fs::remove_file(
        f.dir
            .join("projects/ovzdusie/access/serviceaccounts/cam-north.yaml"),
    )
    .expect("removed");
    let repo = jc_manifest::loader::Repository::load(&f.dir).expect("the repository loads");
    assert_eq!(
        publish(&f.brokers, &accounts_of(&repo), "ovzdusie-cam-north", own),
        (false, "the client is no ServiceAccount")
    );
}

/// The refusals of the ADR's security analysis, each with the rest of the CONNECT valid.
#[test]
fn every_way_a_camera_is_refused() {
    let f = Fixture::new("refused");
    let good = |f: &Fixture| f.token("ovzdusie-cam-north", "mqtt-ovzdusie-cameras", 300);
    let cases: Vec<(&str, Option<&str>, Connect)> = vec![
        (
            "a token for another broker",
            Some(CAMERAS),
            device(
                "ovzdusie-cam-north",
                f.token("ovzdusie-cam-north", "mqtt-ovzdusie-meters", 300),
            ),
        ),
        (
            "an expired token",
            Some(CAMERAS),
            device(
                "ovzdusie-cam-north",
                f.token("ovzdusie-cam-north", "mqtt-ovzdusie-cameras", -120),
            ),
        ),
        (
            "another client id than the token's",
            Some(CAMERAS),
            Connect {
                clientid: "ovzdusie-cam-south".to_owned(),
                ..device("ovzdusie-cam-north", good(&f))
            },
        ),
        (
            "another username than the token's",
            Some(CAMERAS),
            Connect {
                username: "ovzdusie-cam-south".to_owned(),
                ..device("ovzdusie-cam-north", good(&f))
            },
        ),
        (
            "an account of another project, same name",
            Some(CAMERAS),
            device(
                "espoo-cam-north",
                f.token("espoo-cam-north", "mqtt-ovzdusie-cameras", 300),
            ),
        ),
        (
            "an account with no role on this broker",
            Some(CAMERAS),
            device(
                "ovzdusie-cam-idle",
                f.token("ovzdusie-cam-idle", "mqtt-ovzdusie-cameras", 300),
            ),
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
        ),
        ("no mesh identity", None, device("ovzdusie-cam-north", good(&f))),
        (
            "a broker asking with another broker's token audience",
            Some(METERS),
            device("ovzdusie-cam-north", good(&f)),
        ),
        (
            "a caller that is no broker",
            Some("context-gateway.jc.serviceaccount.identity.linkerd.cluster.local"),
            device("ovzdusie-cam-north", good(&f)),
        ),
    ];
    for (why, caller, connect) in cases {
        let answer = f.decide(caller, &connect);
        assert_eq!(
            answer,
            json!({"result": "deny", "superuser": false}),
            "{why}"
        );
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
        let response = internal_router(Arc::clone(&gateway), None, None)
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

/// T-5560: the publish check answers on the internal router with the broker's cache time on an
/// allow and none on a deny.
#[tokio::test]
async fn the_publish_check_answers_on_the_internal_router_with_its_cache_time() {
    let f = Fixture::new("acl-router");
    let gateway = Gateway::new(
        Broker::new("http://broker.invalid:1026"),
        Box::new(PolicyPdp),
        "banskabystrica.sk",
    )
    .authenticate(Arc::new(f.realm.verifier()), f.accounts.clone(), None);
    gateway.replace_brokers(f.brokers.clone());
    let gateway = Arc::new(gateway);
    for (topic, result, cache) in [
        (
            "cameras/ovzdusie-cam-north/frame",
            "allow",
            Some(PUBLISH_CACHE_MS.to_string()),
        ),
        ("cameras/ovzdusie-cam-soon/frame", "deny", None),
    ] {
        let body = json!({
            "access": "2", "clientid": "ovzdusie-cam-north",
            "username": "ovzdusie-cam-north", "topic": topic
        })
        .to_string();
        let request = Request::post("/internal/mqtt/acl")
            .header("content-type", "application/json")
            .header("l5d-client-id", CAMERAS)
            .body(Body::from(body))
            .expect("a request");
        let response = internal_router(Arc::clone(&gateway), None, None)
            .oneshot(request)
            .await
            .expect("answers");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get("x-cache")
                .map(|v| v.to_str().expect("ascii").to_owned()),
            cache,
            "{topic}"
        );
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .expect("a body");
        let answer: Value = serde_json::from_slice(&bytes).expect("json");
        assert_eq!(answer, json!({"result": result}), "{topic}");
    }
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
        let app = internal_router(Arc::clone(&gateway), host.map(str::to_owned), None);
        let body = json!({
            "clientid": "jc-ovzdusie-frames", "username": "pipeline-runner",
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
