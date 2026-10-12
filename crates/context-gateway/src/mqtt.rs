//! The CONNECT check of the MQTT edge brokers (PL-75, PL-76, ADR-N-048).
//!
//! Each broker is the unmodified RMQTT image; its `rmqtt-auth-http` plugin posts every CONNECT
//! here, on the gateway's internal listener, and gets back `allow` with the client's ACL or
//! `deny`. The broker asking is named by its mesh identity, never by the request: no broker can
//! ask with another broker's roles. The password is a Keycloak access token for a device, and
//! empty for the pipeline runner, which only the internal listener of a broker reaches.

use crate::app::Gateway;
use crate::auth::accounts::ServiceAccounts;
use crate::auth::token::Verifier;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::Json;
use jc_core::kinds::mqtt_broker::{release_name, MqttRole, RUNNER_USERNAME};
use jc_core::kinds::{DataSourceSpec, MqttBrokerSpec};
use jcctl::loader::Repository;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;

/// The header the Linkerd inbound proxy sets to the mTLS identity of the caller.
pub const MESH_IDENTITY: &str = "l5d-client-id";

/// One broker the repository declares, as the CONNECT check reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Broker {
    /// The broker's project.
    pub project: String,
    /// Its roles, from the manifest.
    pub roles: Vec<MqttRole>,
    /// The topic filters of the DataSources of its project that name it, which the runner may
    /// subscribe to (PL-76).
    pub runner_topics: Vec<String>,
}

/// Every broker, by release name `mqtt-{project}-{name}`, which is also its Keycloak client
/// (the token audience) and its Kubernetes ServiceAccount (its mesh identity).
pub type Brokers = HashMap<String, Broker>;

/// The brokers of a repository with the topics their pipelines read. A manifest the kind
/// refuses is left out with a line in the log; `jcctl validate` refuses it before merge.
pub fn brokers_of(repo: &Repository) -> Brokers {
    let mut brokers = Brokers::new();
    for (id, resource) in repo.iter() {
        if id.kind != "MqttBroker" {
            continue;
        }
        let project = id.namespace.clone().unwrap_or_default();
        let spec = serde_json::from_value::<MqttBrokerSpec>(resource.manifest.spec.clone())
            .map_err(|error| error.to_string())
            .and_then(|spec| spec.validate().map(|()| spec).map_err(|e| e.to_string()));
        match spec {
            Ok(spec) => {
                brokers.insert(
                    release_name(&project, &id.name),
                    Broker {
                        project,
                        roles: spec.roles,
                        runner_topics: Vec::new(),
                    },
                );
            }
            Err(error) => tracing::warn!(broker = %id.name, %error, "the MqttBroker is not served"),
        }
    }
    for (id, resource) in repo.iter() {
        if id.kind != "DataSource" {
            continue;
        }
        let Ok(source) = serde_json::from_value::<DataSourceSpec>(resource.manifest.spec.clone())
        else {
            continue;
        };
        let Some(mqtt) = source.mqtt else { continue };
        let Some(broker) = mqtt.broker_ref else {
            continue;
        };
        let project = id.namespace.clone().unwrap_or_default();
        if let Some(broker) = brokers.get_mut(&release_name(&project, &broker.name)) {
            broker.runner_topics.extend(mqtt.topics);
        }
    }
    for broker in brokers.values_mut() {
        broker.runner_topics.sort();
        broker.runner_topics.dedup();
    }
    brokers
}

/// What `rmqtt-auth-http` posts (`http_auth_req.params`, JSON).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Connect {
    /// The MQTT client id.
    #[serde(default)]
    pub clientid: String,
    /// The MQTT username.
    #[serde(default)]
    pub username: String,
    /// The MQTT password: an access token, or empty for the runner.
    #[serde(default)]
    pub password: String,
    /// The client's address as the broker sees it: the public one through Traefik's PROXY
    /// protocol, a private or loopback one inside the cluster.
    #[serde(default)]
    pub ipaddr: String,
}

/// RMQTT's answer format (`docs/en_US/auth-http.md`, `perm-list.md`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Answer {
    /// `allow` or `deny`; never `ignore`, which would hand the decision to the next plugin.
    pub result: &'static str,
    /// Always false: no client bypasses its ACL.
    pub superuser: bool,
    /// When the broker drops the connection: the token's `exp`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expire_at: Option<i64>,
    /// The client's ACL, evaluated by the broker on every publish and subscribe.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub acl: Vec<Rule>,
}

/// One ACL rule of the answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Rule {
    /// Always `allow`: the broker's own last rule denies the rest.
    pub permission: &'static str,
    /// `publish` or `subscribe`.
    pub action: &'static str,
    /// A topic filter, `${clientid}` left for the broker to bind.
    pub topic: String,
    /// QoS 0 and 1; the listener refuses 2 anyway.
    pub qos: [u8; 2],
    /// Publish only: a retained message is never allowed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retain: Option<bool>,
}

impl Answer {
    fn deny() -> Self {
        Self {
            result: "deny",
            superuser: false,
            expire_at: None,
            acl: Vec::new(),
        }
    }

    fn allow(acl: Vec<Rule>, expire_at: Option<i64>) -> Self {
        Self {
            result: "allow",
            superuser: false,
            expire_at,
            acl,
        }
    }
}

fn rule(action: &'static str, topic: &str) -> Rule {
    Rule {
        permission: "allow",
        action,
        topic: topic.to_owned(),
        qos: [0, 1],
        retain: (action == "publish").then_some(false),
    }
}

/// The broker a mesh identity names: Linkerd's `{serviceaccount}.{namespace}.serviceaccount.
/// identity.…` whose first label is the broker's release name.
pub fn broker_of(identity: &str) -> &str {
    identity.split('.').next().unwrap_or_default()
}

/// Decides one CONNECT (PL-75, PL-76). `caller` is the mesh identity of the broker asking.
pub fn decide(
    brokers: &Brokers,
    verifier: Option<&Verifier>,
    accounts: &ServiceAccounts,
    caller: Option<&str>,
    connect: &Connect,
) -> (Answer, &'static str) {
    let Some(release) = caller.map(broker_of) else {
        return (Answer::deny(), "no mesh identity");
    };
    let Some(broker) = brokers.get(release) else {
        return (
            Answer::deny(),
            "the caller is no broker this repository declares",
        );
    };
    if connect.password.is_empty() {
        let inside = connect
            .ipaddr
            .parse::<IpAddr>()
            .is_ok_and(|ip| is_inside(ip.to_canonical()));
        if connect.username != RUNNER_USERNAME || !inside {
            return (Answer::deny(), "no token");
        }
        if broker.runner_topics.is_empty() {
            return (Answer::deny(), "no DataSource reads this broker");
        }
        let acl = broker
            .runner_topics
            .iter()
            .map(|topic| rule("subscribe", topic))
            .collect();
        return (Answer::allow(acl, None), "the pipeline runner");
    }
    let Some(verifier) = verifier else {
        return (Answer::deny(), "no realm configured");
    };
    let Ok(claims) = verifier.verify(&connect.password, &[release.to_owned()]) else {
        return (Answer::deny(), "the token does not verify for this broker");
    };
    let Some(azp) = claims.azp.as_deref() else {
        return (Answer::deny(), "the token names no client");
    };
    if azp != connect.username || azp != connect.clientid {
        return (
            Answer::deny(),
            "the username or client id is not the token's client",
        );
    }
    let Some(account) = accounts.resolve(azp) else {
        return (Answer::deny(), "the client is no ServiceAccount");
    };
    if account.project != broker.project {
        return (Answer::deny(), "the account belongs to another project");
    }
    let mut acl = Vec::new();
    for role in broker
        .roles
        .iter()
        .filter(|role| role.service_accounts.contains(&account.name))
    {
        acl.extend(role.publish.iter().map(|topic| rule("publish", topic)));
        acl.extend(role.subscribe.iter().map(|topic| rule("subscribe", topic)));
    }
    if acl.is_empty() {
        return (Answer::deny(), "the account holds no role of this broker");
    }
    (Answer::allow(acl, Some(claims.exp)), "a device")
}

/// A private or loopback address: inside the cluster, never a client Traefik forwarded.
fn is_inside(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_private() || v4.is_loopback(),
        IpAddr::V6(v6) => v6.is_loopback() || (v6.segments()[0] & 0xfe00) == 0xfc00,
    }
}

/// `POST /internal/mqtt/auth`, on the internal listener only. Always 200 with `allow` or `deny`:
/// a non-2xx would read as `ignore` to the broker (`deny_if_error` covers transport faults only).
pub async fn auth(
    State(gateway): State<Arc<Gateway>>,
    headers: HeaderMap,
    Json(connect): Json<Connect>,
) -> Json<Answer> {
    let caller = headers
        .get(MESH_IDENTITY)
        .and_then(|value| value.to_str().ok());
    let brokers = gateway.brokers();
    let accounts = gateway.service_accounts();
    let (answer, why) = decide(
        &brokers,
        gateway.verifier.as_deref(),
        &accounts,
        caller,
        &connect,
    );
    // The client id and the outcome, never the password.
    tracing::info!(
        broker = caller.map(broker_of).unwrap_or_default(),
        clientid = %connect.clientid,
        result = answer.result,
        why,
        "mqtt connect"
    );
    Json(answer)
}

/// The internal listener's router: the CONNECT check and nothing else.
pub fn internal_router(gateway: Arc<Gateway>) -> axum::Router {
    axum::Router::new()
        .route("/internal/mqtt/auth", axum::routing::post(auth))
        .with_state(gateway)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mesh_identity_names_its_broker_by_its_first_label() {
        assert_eq!(
            broker_of(
                "mqtt-helsinki-cameras.jc-mqtt.serviceaccount.identity.linkerd.cluster.local"
            ),
            "mqtt-helsinki-cameras"
        );
        assert_eq!(broker_of(""), "");
    }

    #[test]
    fn inside_is_private_or_loopback_in_either_family() {
        for inside in [
            "10.42.0.7",
            "127.0.0.1",
            "192.168.1.2",
            "::1",
            "fd00::1",
            "::ffff:10.0.0.1",
        ] {
            let ip: IpAddr = inside.parse().expect("an address");
            assert!(is_inside(ip.to_canonical()), "{inside}");
        }
        for outside in ["8.8.8.8", "2001:db8::1", "::ffff:8.8.8.8"] {
            let ip: IpAddr = outside.parse().expect("an address");
            assert!(!is_inside(ip.to_canonical()), "{outside}");
        }
    }

    #[test]
    fn the_answer_reads_as_rmqtt_expects_it() {
        let answer = Answer::allow(
            vec![
                rule("publish", "cameras/${clientid}/#"),
                rule("subscribe", "x/${clientid}"),
            ],
            Some(1_900_000_000),
        );
        assert_eq!(
            serde_json::to_value(&answer).expect("serializes"),
            serde_json::json!({
                "result": "allow", "superuser": false, "expire_at": 1_900_000_000,
                "acl": [
                    {"permission": "allow", "action": "publish", "topic": "cameras/${clientid}/#", "qos": [0, 1], "retain": false},
                    {"permission": "allow", "action": "subscribe", "topic": "x/${clientid}", "qos": [0, 1]}
                ]
            })
        );
        assert_eq!(
            serde_json::to_value(Answer::deny()).expect("serializes"),
            serde_json::json!({"result": "deny", "superuser": false})
        );
    }
}
