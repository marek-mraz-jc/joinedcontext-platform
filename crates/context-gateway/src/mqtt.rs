//! The CONNECT check of the MQTT edge brokers (PL-75, PL-76, ADR-N-048).
//!
//! Each broker is the unmodified RMQTT image; its `rmqtt-auth-http` plugin posts every CONNECT
//! here, on the gateway's internal listener, and gets back `allow` with the client's ACL or
//! `deny`. The broker asking is named by its mesh identity, never by the request: no broker can
//! ask with another broker's roles. The password is a Keycloak access token for a device, and
//! empty for the pipeline runner, which is admitted only from the address of one of its own
//! pods (T-4004): a private address alone proves nothing, because a cluster pod that dials the
//! public listener through Traefik arrives with its pod address in the PROXY header.

use crate::app::Gateway;
use crate::auth::accounts::ServiceAccounts;
use crate::auth::token::Verifier;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::Json;
use jc_core::kinds::mqtt_broker::{release_name, MqttRole, RUNNER_USERNAME};
use jc_core::kinds::{DataSourceSpec, MqttBrokerSpec};
use jc_manifest::loader::Repository;
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
#[derive(Clone, Default, Deserialize)]
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
    /// The client's address as the broker sees it: the one Traefik's PROXY protocol carries
    /// on the public listener, the caller's pod address on the internal one.
    #[serde(default)]
    pub ipaddr: String,
}

/// Everything but the password, a person's access token (T-4320).
impl std::fmt::Debug for Connect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Connect")
            .field("clientid", &self.clientid)
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .field("ipaddr", &self.ipaddr)
            .finish()
    }
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

/// Decides one CONNECT (PL-75, PL-76). `caller` is the mesh identity of the broker asking;
/// `runner` the addresses of the pipeline runner's pods, the only ones a tokenless CONNECT is
/// admitted from.
pub fn decide(
    brokers: &Brokers,
    verifier: Option<&Verifier>,
    accounts: &ServiceAccounts,
    caller: Option<&str>,
    runner: &[IpAddr],
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
        let from_runner = connect
            .ipaddr
            .parse::<IpAddr>()
            .is_ok_and(|ip| runner.contains(&ip.to_canonical()));
        if connect.username != RUNNER_USERNAME || !from_runner {
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

/// The addresses `host` resolves to: the runner's headless Service lists one per pod. A name
/// that does not resolve admits nobody.
pub async fn runner_addresses(host: &str) -> Vec<IpAddr> {
    match tokio::net::lookup_host((host, 0)).await {
        Ok(found) => found.map(|address| address.ip().to_canonical()).collect(),
        Err(error) => {
            tracing::warn!(%error, host, "the pipeline runner's pods do not resolve");
            Vec::new()
        }
    }
}

/// What the internal listener's routes share.
#[derive(Clone)]
struct Internal {
    gateway: Arc<Gateway>,
    runner_host: Option<Arc<str>>,
}

/// `POST /internal/mqtt/auth`, on the internal listener only. Always 200 with `allow` or `deny`:
/// a non-2xx would read as `ignore` to the broker (`deny_if_error` covers transport faults only).
async fn auth(
    State(Internal {
        gateway,
        runner_host,
    }): State<Internal>,
    headers: HeaderMap,
    Json(connect): Json<Connect>,
) -> Json<Answer> {
    let caller = headers
        .get(MESH_IDENTITY)
        .and_then(|value| value.to_str().ok());
    let brokers = gateway.brokers();
    let accounts = gateway.service_accounts();
    // Resolved per tokenless CONNECT only: a runner reconnects rarely, a pod moves its address.
    let runner = match &runner_host {
        Some(host) if connect.password.is_empty() && connect.username == RUNNER_USERNAME => {
            runner_addresses(host).await
        }
        _ => Vec::new(),
    };
    let (answer, why) = decide(
        &brokers,
        gateway.verifier.as_deref(),
        &accounts,
        caller,
        &runner,
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

/// The internal listener's router: the CONNECT check and nothing else. `runner_host` names
/// the pipeline runner's headless Service; without one no tokenless CONNECT is admitted.
pub fn internal_router(gateway: Arc<Gateway>, runner_host: Option<String>) -> axum::Router {
    axum::Router::new()
        .route("/internal/mqtt/auth", axum::routing::post(auth))
        .with_state(Internal {
            gateway,
            runner_host: runner_host.map(Arc::from),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_connect_debug_never_shows_the_password() {
        let connect = Connect {
            username: "anna".into(),
            password: "s3cr3t-test".into(),
            ..Connect::default()
        };
        let text = format!("{connect:?}");
        assert!(!text.contains("s3cr3t-test"), "{text}");
        assert!(
            text.contains("anna") && text.contains("<redacted>"),
            "{text}"
        );
    }

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

    #[tokio::test]
    async fn the_runner_is_the_addresses_its_name_resolves_to_and_nothing_else() {
        assert!(runner_addresses("localhost")
            .await
            .contains(&"127.0.0.1".parse::<IpAddr>().expect("an address")));
        assert!(runner_addresses("no-such-runner.invalid").await.is_empty());
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
