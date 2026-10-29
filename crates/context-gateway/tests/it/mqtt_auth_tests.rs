//! The MQTT edge brokers' CONNECT check at `/internal/mqtt/auth` (T-3837, PL-75, PL-76,
//! ADR-N-048). One test per row of the ADR's security analysis that the gateway holds: a camera
//! publishes under its own client id only, a token for another broker or another project, an
//! expired one and one under another client id are refused, a broker asks with its own mesh
//! identity only, and the runner connects without a token from inside the cluster alone.

use crate::common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use context_gateway::app::{router, Gateway};
use context_gateway::auth::accounts::accounts_of;
use context_gateway::mqtt::{
    brokers_of, decide, internal_router, publish_check, Access, Brokers, Connect, Pods,
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
const ESPOO: &str = "mqtt-espoo-cameras.jc-mqtt.serviceaccount.identity.linkerd.cluster.local";

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
        // The meters broker admits devices from 203.0.113.0/24 alone (PF-129).
        let sources = if name == "meters" {
            "  allowedSources: [203.0.113.0/24, \"2001:db8::/48\"]\n"
        } else {
            ""
        };
        write(
            &dir,
            &format!("projects/ovzdusie/mqttbrokers/{name}.yaml"),
            &format!("apiVersion: joinedcontext.com/v1alpha1\nkind: MqttBroker\nmetadata:\n  name: {name}\n  namespace: ovzdusie\nspec:\n{sources}  mode: relay\n  roles:\n    - name: camera\n      serviceAccounts: {accounts}\n      publish: [\"cameras/${{clientid}}/#\"]\n      subscribe: [\"commands/${{clientid}}/+\"]\n"),
        );
    }
    for (name, mode) in [("probe", "capture"), ("plain", "ingest")] {
        write(
            &dir,
            &format!("projects/ovzdusie/mqttbrokers/{name}.yaml"),
            &format!("apiVersion: joinedcontext.com/v1alpha1\nkind: MqttBroker\nmetadata:\n  name: {name}\n  namespace: ovzdusie\nspec:\n  mode: {mode}\n  roles:\n    - name: sensor\n      serviceAccounts: [cam-north]\n      sharedTopics: true\n      reason: \"a gateway forwards for every sensor\"\n      publish: [\"air/${{clientid}}/pm\", \"air/status\"]\n"),
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
                brokers: vec![BROKER_POD.parse().expect("an address")],
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

/// A broker pod's address, which the brokers' headless Service lists in these tests: under the
/// mesh the broker reports every client of its internal listener at this address (T-5893).
const BROKER_POD: &str = "10.42.0.107";

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

/// T-5893: on dev the broker reports the runner at the broker's own address, Linkerd's forward
/// on listener 1883, which admits only the runner's and the Portal's mesh identities: the runner
/// gets its topics from there, and its name without a token from any other address (a client
/// through Traefik's PROXY header, another pod) or another project's broker is still refused.
#[test]
fn the_runner_is_admitted_from_its_brokers_own_address_as_the_mesh_forwards_it() {
    let f = Fixture::new("runner-mesh");
    let answer = f.decide(Some(CAMERAS), &runner(BROKER_POD));
    assert_eq!(answer["result"], "allow", "{answer}");
    assert!(answer.get("expire_at").is_none(), "{answer}");
    assert_eq!(
        answer["acl"],
        json!([
            {"permission": "allow", "action": "subscribe", "topic": "cameras/+/frame", "qos": [0, 1]},
            {"permission": "allow", "action": "subscribe", "topic": "cameras/+/status", "qos": [0, 1]}
        ])
    );
    let deny = json!({"result": "deny", "superuser": false});
    for elsewhere in ["203.0.113.7", "10.42.0.108", "10.42.0.99"] {
        assert_eq!(
            f.decide(Some(CAMERAS), &runner(elsewhere)),
            deny,
            "{elsewhere}"
        );
    }
    let someone = Connect {
        username: "ovzdusie-cam-north".to_owned(),
        ..runner(BROKER_POD)
    };
    assert_eq!(f.decide(Some(CAMERAS), &someone), deny);
    assert_eq!(f.decide(None, &runner(BROKER_POD)), deny);
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
    // Under the mesh the capture arrives at the broker's own address, as the runner does (T-5893).
    assert_eq!(
        f.decide(Some(PROBE), &capture(BROKER_POD))["result"],
        "allow"
    );
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
        let response = internal_router(Arc::clone(&gateway), None, None, None, None)
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

/// T-5560, T-5562: the publish check answers on the internal router with no cache time, so the
/// broker asks for every publish; a client above its broker's `messagesPerSecond` (10 by
/// default) is refused at once, which disconnects it.
#[tokio::test]
async fn the_publish_check_answers_each_publish_and_refuses_a_flood() {
    let f = Fixture::new("acl-router");
    let gateway = Gateway::new(
        Broker::new("http://broker.invalid:1026"),
        Box::new(PolicyPdp),
        "banskabystrica.sk",
    )
    .authenticate(Arc::new(f.realm.verifier()), f.accounts.clone(), None);
    gateway.replace_brokers(f.brokers.clone());
    let app = internal_router(Arc::new(gateway), None, None, None, None);
    let ask = |topic: &str| {
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
        let app = app.clone();
        async move {
            let response = app.oneshot(request).await.expect("answers");
            assert_eq!(response.status(), StatusCode::OK);
            assert!(response.headers().get("x-cache").is_none(), "no cache time");
            let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
                .await
                .expect("a body");
            let answer: Value = serde_json::from_slice(&bytes).expect("json");
            answer["result"].as_str().expect("a result").to_owned()
        }
    };
    assert_eq!(ask("cameras/ovzdusie-cam-soon/frame").await, "deny");
    let mut answers = Vec::new();
    for _ in 0..30 {
        answers.push(ask("cameras/ovzdusie-cam-north/frame").await);
    }
    assert!(answers[..10].iter().all(|a| a == "allow"), "{answers:?}");
    assert!(
        answers.iter().any(|a| a == "deny"),
        "a flood of 30 passed: {answers:?}"
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
        let app = internal_router(
            Arc::clone(&gateway),
            host.map(str::to_owned),
            None,
            None,
            None,
        );
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

/// T-5893: the internal router resolves the brokers' headless Service and admits the tokenless
/// runner from what it lists; without the host, or for a caller that is no declared broker, the
/// runner's name stays refused there.
#[tokio::test]
async fn the_router_admits_the_runner_from_the_brokers_own_pods_only() {
    let f = Fixture::new("runner-broker-pods");
    let gateway = Gateway::new(
        Broker::new("http://broker.invalid:1026"),
        Box::new(PolicyPdp),
        "banskabystrica.sk",
    )
    .authenticate(Arc::new(f.realm.verifier()), f.accounts.clone(), None);
    gateway.replace_brokers(f.brokers.clone());
    let gateway = Arc::new(gateway);
    let ask = |host: Option<&str>, caller: &'static str, ipaddr: &str| {
        let app = internal_router(
            Arc::clone(&gateway),
            Some("no-such-runner.invalid".to_owned()),
            None,
            host.map(str::to_owned),
            None,
        );
        let body = json!({
            "clientid": "jc-ovzdusie-frames", "username": "pipeline-runner-ovzdusie",
            "password": "", "ipaddr": ipaddr
        })
        .to_string();
        async move {
            let request = Request::post("/internal/mqtt/auth")
                .header("content-type", "application/json")
                .header("l5d-client-id", caller)
                .body(Body::from(body))
                .expect("a request");
            let response = app.oneshot(request).await.expect("answers");
            let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
                .await
                .expect("a body");
            serde_json::from_slice::<Value>(&bytes).expect("json")["result"].clone()
        }
    };
    const STRANGER: &str = "mqtt-elsewhere.jc-mqtt.serviceaccount.identity.linkerd.cluster.local";
    assert_eq!(ask(Some("localhost"), CAMERAS, "127.0.0.1").await, "allow");
    assert_eq!(ask(Some("localhost"), CAMERAS, BROKER_POD).await, "deny");
    assert_eq!(ask(None, CAMERAS, "127.0.0.1").await, "deny");
    assert_eq!(ask(Some("localhost"), STRANGER, "127.0.0.1").await, "deny");
    assert_eq!(
        ask(Some("no-such-broker.invalid"), CAMERAS, "127.0.0.1").await,
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

/// PF-129: on a broker with allowedSources a device's token admits it only from an address
/// inside a listed network, as the PROXY header carried it; outside, or unreadable, it is refused
/// and recorded as `source_address` before the token is read.
#[test]
fn a_device_outside_the_brokers_allowed_sources_is_refused() {
    let f = Fixture::new("sources");
    let at = |ipaddr: &str| {
        let token = f.token("ovzdusie-cam-idle", "mqtt-ovzdusie-meters", 300);
        let mut connect = device("ovzdusie-cam-idle", token);
        connect.ipaddr = ipaddr.to_owned();
        f.decide_why(Some(METERS), &connect)
    };
    for inside in ["203.0.113.7", "::ffff:203.0.113.200", "2001:db8::9"] {
        let (answer, why) = at(inside);
        assert_eq!(answer["result"], "allow", "{inside}: {answer}");
        assert_eq!(why, None);
    }
    for outside in ["198.51.100.7", "2001:db9::1", "", "not-an-address"] {
        let (answer, why) = at(outside);
        assert_eq!(answer["result"], "deny", "{outside}: {answer}");
        assert_eq!(why, Some("source_address"), "{outside}");
    }
    let mut forged = device("ovzdusie-cam-idle", "not-a-token".to_owned());
    forged.ipaddr = "203.0.113.7".to_owned();
    assert_eq!(f.decide_why(Some(METERS), &forged).1, Some("invalid_token"));
    // A broker without the list keeps admitting any address.
    let token = f.token("ovzdusie-cam-north", "mqtt-ovzdusie-cameras", 300);
    let mut anywhere = device("ovzdusie-cam-north", token);
    anywhere.ipaddr = "198.51.100.7".to_owned();
    assert_eq!(f.decide(Some(CAMERAS), &anywhere)["result"], "allow");
}

const AIR: &str = "mqtt-ovzdusie-air.jc-mqtt.serviceaccount.identity.linkerd.cluster.local";

/// An account holding `roles` (YAML list items, each indented by four spaces).
fn bound_account(dir: &Path, project: &str, name: &str, roles: &str) {
    write(
        dir,
        &format!("projects/{project}/access/serviceaccounts/{name}.yaml"),
        &format!("apiVersion: joinedcontext.com/v1alpha1\nkind: ServiceAccount\nmetadata:\n  name: {name}\n  namespace: {project}\nspec:\n  owner:\n    user: demo.steward\n  purpose: \"air sensors\"\n  roles:\n{roles}  credentials:\n    - kind: oauth-client\n      name: default\n"),
    );
}

/// The brokers `air` (role air-sensor) and `noise` (role camera) of `ovzdusie`, which list no
/// account: every holder is a ServiceAccount binding (T-5872, ADR-N-048 §2.13).
fn bound_fixture(test: &str) -> Fixture {
    let dir = std::env::temp_dir().join(format!("gateway-mqtt-{test}"));
    let _ = std::fs::remove_dir_all(&dir);
    let until = |seconds: i64| {
        format!(
            "      validity:\n        notAfter: \"{}\"\n",
            (chrono::Utc::now() + chrono::Duration::seconds(seconds)).to_rfc3339()
        )
    };
    let grant = |role: &str, project: &str, validity: &str| {
        format!("    - role: {role}\n      scope:\n        project: {project}\n{validity}")
    };
    bound_account(
        &dir,
        "ovzdusie",
        "air-fleet",
        &grant("mqtt/air/air-sensor", "ovzdusie", &until(100)),
    );
    bound_account(
        &dir,
        "ovzdusie",
        "air-old",
        &grant("mqtt/*/air-sensor", "ovzdusie", &until(-5)),
    );
    bound_account(
        &dir,
        "ovzdusie",
        "noise-fleet",
        &grant("mqtt/noise/air-sensor", "ovzdusie", ""),
    );
    bound_account(
        &dir,
        "espoo",
        "air-fleet",
        &grant("mqtt/air/air-sensor", "espoo", ""),
    );
    for (name, role) in [("air", "air-sensor"), ("noise", "camera")] {
        write(
            &dir,
            &format!("projects/ovzdusie/mqttbrokers/{name}.yaml"),
            &format!("apiVersion: joinedcontext.com/v1alpha1\nkind: MqttBroker\nmetadata:\n  name: {name}\n  namespace: ovzdusie\nspec:\n  roles:\n    - name: {role}\n      publish: [\"air/${{clientid}}/pm10\"]\n"),
        );
    }
    write(
        &dir,
        "projects/espoo/mqttbrokers/air.yaml",
        "apiVersion: joinedcontext.com/v1alpha1\nkind: MqttBroker\nmetadata:\n  name: air\n  namespace: espoo\nspec:\n  roles:\n    - name: air-sensor\n      publish: [\"air/${clientid}/pm10\"]\n",
    );
    let repo = jc_manifest::loader::Repository::load(&dir).expect("the repository loads");
    Fixture {
        realm: common::Realm::new(),
        brokers: brokers_of(&repo),
        accounts: accounts_of(&repo),
        dir,
    }
}

fn publish_on_air(f: &Fixture, clientid: &str) -> (bool, &'static str) {
    publish_check(
        &f.brokers,
        &f.accounts,
        Some(AIR),
        &Access {
            access: "2".into(),
            clientid: clientid.into(),
            username: clientid.into(),
            topic: format!("air/{clientid}/pm10"),
        },
    )
}

/// T-5872: a fleet holds a broker role by a binding on its own ServiceAccount, until the
/// binding's `notAfter`; the broker lists nobody. Another project's account with the same role
/// name, an expired binding and a binding to a role the broker does not declare get nothing.
#[test]
fn a_service_account_binding_admits_a_fleet_until_its_validity_ends() {
    let f = bound_fixture("bindings");
    let token = f.token("ovzdusie-air-fleet", "mqtt-ovzdusie-air", 300);
    let answer = f.decide(Some(AIR), &device("ovzdusie-air-fleet", token));
    assert_eq!(answer["result"], "allow", "{answer}");
    // The CONNECT ends with the binding (100 s), before the token (300 s).
    let expire_at = answer["expire_at"].as_i64().expect("an expiry");
    assert!(expire_at <= common::in_seconds(101), "{answer}");
    assert!(expire_at > common::in_seconds(90), "{answer}");
    assert_eq!(
        publish_on_air(&f, "ovzdusie-air-fleet"),
        (true, "a device's role")
    );

    for (azp, reason, published) in [
        (
            "ovzdusie-air-old",
            "no_role",
            "no role of the account covers the topic",
        ),
        (
            "ovzdusie-noise-fleet",
            "no_role",
            "no role of the account covers the topic",
        ),
        (
            "espoo-air-fleet",
            "other_project",
            "the account belongs to another project",
        ),
    ] {
        let token = f.token(azp, "mqtt-ovzdusie-air", 300);
        let (answer, why) = f.decide_why(Some(AIR), &device(azp, token));
        assert_eq!(answer["result"], "deny", "{azp}: {answer}");
        assert_eq!(why, Some(reason), "{azp}");
        assert_eq!(publish_on_air(&f, azp), (false, published), "{azp}");
    }
}

/// T-5872: the gateway serves a broker only when the one jc-core validator admits it, over the
/// same table jc-core and the Portal run.
#[test]
fn the_gateway_serves_only_the_brokers_the_shared_validator_admits() {
    #[derive(serde::Deserialize)]
    struct Case {
        case: String,
        spec: Value,
        refused: Option<String>,
    }
    let cases: Vec<Case> =
        serde_json::from_str(include_str!("../../../jc-core/src/mqtt/topics_cases.json"))
            .expect("the table is JSON");
    for (i, case) in cases.iter().enumerate() {
        let dir = std::env::temp_dir().join(format!("gateway-mqtt-table-{i}"));
        let _ = std::fs::remove_dir_all(&dir);
        let manifest = json!({
            "apiVersion": "joinedcontext.com/v1alpha1",
            "kind": "MqttBroker",
            "metadata": {"name": "air", "namespace": "ovzdusie"},
            "spec": case.spec,
        });
        write(
            &dir,
            "projects/ovzdusie/mqttbrokers/air.yaml",
            &serde_json::to_string(&manifest).expect("json"),
        );
        let served = jc_manifest::loader::Repository::load(&dir)
            .map(|repo| brokers_of(&repo).contains_key("mqtt-ovzdusie-air"))
            .unwrap_or(false);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(served, case.refused.is_none(), "{}", case.case);
    }
}

const DOORS: &str = "mqtt-ovzdusie-doors.jc-mqtt.serviceaccount.identity.linkerd.cluster.local";
const DOOR: &str = "mqtt-ovzdusie-doors-cert.jc-mqtt.serviceaccount.identity.linkerd.cluster.local";
const ONLY_DOOR: &str =
    "mqtt-ovzdusie-certonly-cert.jc-mqtt.serviceaccount.identity.linkerd.cluster.local";
const ONLY: &str = "mqtt-ovzdusie-certonly.jc-mqtt.serviceaccount.identity.linkerd.cluster.local";

/// The repository of [`repository`] with two brokers whose devices may use client certificates
/// (ADR-N-048 §2.12, T-5875): `doors` with tokens too, `certonly` with certificates alone. The
/// allowance that lets the project name them is the Portal's to check, not the gateway's.
fn door_fixture(test: &str) -> Fixture {
    let dir = repository(test);
    for (name, auth) in [
        ("doors", "[token, clientCertificate]"),
        ("certonly", "[clientCertificate]"),
    ] {
        write(
            &dir,
            &format!("projects/ovzdusie/mqttbrokers/{name}.yaml"),
            &format!("apiVersion: joinedcontext.com/v1alpha1\nkind: MqttBroker\nmetadata:\n  name: {name}\n  namespace: ovzdusie\nspec:\n  mode: relay\n  deviceAuth: {auth}\n  roles:\n    - name: camera\n      serviceAccounts: [cam-north, cam-gone]\n      publish: [\"cameras/${{clientid}}/#\"]\n      subscribe: [\"commands/${{clientid}}/+\"]\n"),
        );
    }
    let repo = jc_manifest::loader::Repository::load(&dir).expect("the repository loads");
    Fixture {
        realm: common::Realm::new(),
        brokers: brokers_of(&repo),
        accounts: accounts_of(&repo),
        dir,
    }
}

/// What RMQTT posts for a device whose verified certificate has the subject `dn`: the DN as the
/// username, the client id the device chose and no password.
fn certified(dn: &str, clientid: &str) -> Connect {
    Connect {
        clientid: clientid.to_owned(),
        username: dn.to_owned(),
        password: String::new(),
        ipaddr: "203.0.113.7".to_owned(),
    }
}

/// T-5875 (T-4861 option (a)): the certificate door is a release of its own, and only its mesh
/// identity vouches for a certificate RMQTT verified. A device whose common name is a
/// ServiceAccount of the broker's project, under that account's client id, gets the account's
/// roles for an hour at most; a certificate RMQTT refused (another CA, expired) never reaches the
/// gateway (tests/test_mqtt_brokers.py in the deployment runs that against the pinned RMQTT).
#[test]
fn a_certificate_device_is_its_common_names_account_for_an_hour() {
    let f = door_fixture("certificate-good");
    let answer = f.decide(
        Some(DOOR),
        &certified("O=ovzdusie, CN=cam-north", "ovzdusie-cam-north"),
    );
    assert_eq!(answer["result"], "allow", "{answer}");
    assert_eq!(answer["acl"][0]["topic"], "commands/${clientid}/+");
    let expire_at = answer["expire_at"].as_i64().expect("an expiry");
    let hour = chrono::Utc::now().timestamp() + 3600;
    assert!((hour - 5..=hour).contains(&expire_at), "{expire_at}");
    // The same broker's token door still admits a token (deviceAuth names both).
    let token = f.token("ovzdusie-cam-north", "mqtt-ovzdusie-doors", 300);
    assert_eq!(
        f.decide(Some(DOORS), &device("ovzdusie-cam-north", token))["result"],
        "allow"
    );
    // A publish through the door is checked on the DN's account, as a token device's is.
    let publish = |dn: &str, clientid: &str| {
        publish_check(
            &f.brokers,
            &f.accounts,
            Some(DOOR),
            &Access {
                access: "2".into(),
                clientid: clientid.into(),
                username: dn.into(),
                topic: format!("cameras/{clientid}/frame"),
            },
        )
    };
    assert_eq!(
        publish("O=ovzdusie, CN=cam-north", "ovzdusie-cam-north"),
        (true, "a device's role")
    );
    assert_eq!(
        publish("CN=cam-idle", "ovzdusie-cam-north").1,
        "no device",
        "a DN of another account under this client id"
    );
    assert!(!publish("ovzdusie-cam-north", "ovzdusie-cam-north").0);
}

/// Every way a CONNECT through the door, or a passwordless one beside it, is refused.
#[test]
fn every_way_a_certificate_device_is_refused() {
    let f = door_fixture("certificate-refused");
    let refused = |caller, connect: &Connect, reason| {
        let (answer, why) = f.decide_why(Some(caller), connect);
        assert_eq!(answer["result"], "deny", "{answer}");
        assert_eq!(why, Some(reason), "{}", connect.username);
    };
    // The common name is not the client id's account.
    refused(
        DOOR,
        &certified("CN=cam-north", "ovzdusie-cam-idle"),
        "wrong_client",
    );
    refused(
        DOOR,
        &certified("CN=cam-north", "cam-north"),
        "wrong_client",
    );
    // A certificate without a common name, with two, or with one no account could carry.
    for dn in [
        "O=ovzdusie",
        "CN=cam-north, CN=cam-idle",
        "CN=",
        "CN=Cam North",
        "",
    ] {
        refused(DOOR, &certified(dn, "ovzdusie-cam-north"), "certificate");
    }
    // A password on the door: the door is for certificates; a token goes to the token door.
    let mut with_password = certified("CN=cam-north", "ovzdusie-cam-north");
    with_password.password = f.token("ovzdusie-cam-north", "mqtt-ovzdusie-doors", 300);
    refused(DOOR, &with_password, "certificate");
    // A common name that is no account, an account without a role, one whose credentials ended
    // (the revocation RMQTT 0.24 cannot do by CRL), one of another project.
    refused(
        DOOR,
        &certified("CN=cam-nobody", "ovzdusie-cam-nobody"),
        "unknown_account",
    );
    refused(
        DOOR,
        &certified("CN=cam-idle", "ovzdusie-cam-idle"),
        "no_role",
    );
    refused(
        DOOR,
        &certified("CN=cam-gone", "ovzdusie-cam-gone"),
        "expired",
    );
    // espoo/cam-north is no account of ovzdusie: its client id names espoo.
    refused(
        DOOR,
        &certified("CN=cam-north", "espoo-cam-north"),
        "wrong_client",
    );
    // The token door never takes a passwordless CONNECT for a certificate: the username is the
    // device's to choose there, so the same DN gets nothing.
    let (answer, why) = f.decide_why(
        Some(DOORS),
        &certified("CN=cam-north", "ovzdusie-cam-north"),
    );
    assert_eq!(
        (answer["result"].as_str(), why),
        (Some("deny"), Some("no_token"))
    );
    // A broker that names certificates only refuses a token on its token door, and serves its
    // certificate door; a broker without clientCertificate has no door at all.
    let token = f.token("ovzdusie-cam-north", "mqtt-ovzdusie-certonly", 300);
    refused(
        ONLY,
        &device("ovzdusie-cam-north", token),
        "token_not_allowed",
    );
    assert_eq!(
        f.decide(
            Some(ONLY_DOOR),
            &certified("CN=cam-north", "ovzdusie-cam-north")
        )["result"],
        "allow"
    );
    assert!(!f.brokers.contains_key("mqtt-ovzdusie-cameras-cert"));
    // The door outside allowedSources is refused like the token door.
    let mut far = certified("CN=cam-north", "ovzdusie-cam-north");
    far.ipaddr = "not an address".into();
    let door = f
        .brokers
        .get("mqtt-ovzdusie-doors-cert")
        .expect("the door")
        .clone();
    let mut brokers = f.brokers.clone();
    brokers.insert(
        "mqtt-ovzdusie-doors-cert".into(),
        context_gateway::mqtt::Broker {
            sources: vec![
                jc_core::kinds::mqtt_broker::SourceRange::parse("203.0.113.0/24").expect("a range"),
            ],
            ..door
        },
    );
    let (answer, why) = decide(
        &brokers,
        None,
        &f.accounts,
        Some(DOOR),
        &Pods::default(),
        &far,
    );
    assert_eq!(answer.result, "deny");
    assert_eq!(
        why.refused.map(|reason| reason.code()),
        Some("source_address")
    );
}

/// A broker named `<name>-cert` holds the release another broker's door would take: that door
/// is not served, and the release stays the other broker's token door.
#[test]
fn a_door_whose_release_is_another_brokers_is_not_served() {
    let dir = repository("certificate-collision");
    for (name, auth) in [("x", "[clientCertificate]"), ("x-cert", "[token]")] {
        write(
            &dir,
            &format!("projects/ovzdusie/mqttbrokers/{name}.yaml"),
            &format!("apiVersion: joinedcontext.com/v1alpha1\nkind: MqttBroker\nmetadata:\n  name: {name}\n  namespace: ovzdusie\nspec:\n  deviceAuth: {auth}\n  roles:\n    - name: camera\n      serviceAccounts: [cam-north]\n      publish: [\"cameras/${{clientid}}/#\"]\n"),
        );
    }
    let repo = jc_manifest::loader::Repository::load(&dir).expect("the repository loads");
    let brokers = brokers_of(&repo);
    let _ = std::fs::remove_dir_all(&dir);
    let entry = brokers
        .get("mqtt-ovzdusie-x-cert")
        .expect("the token broker");
    assert_eq!(
        (entry.name.as_str(), entry.certificate_door),
        ("x-cert", false)
    );
}
