//! The CONNECT check of the MQTT edge brokers (PL-75, PL-76, ADR-N-048).
//!
//! Each broker is the unmodified RMQTT image; its `rmqtt-auth-http` plugin posts every CONNECT
//! here, on the gateway's internal listener, and gets back `allow` with the client's ACL or
//! `deny`. The broker asking is named by its mesh identity, never by the request: no broker can
//! ask with another broker's roles. The password is a Keycloak access token for a device, and
//! empty for the pipeline runner, which is admitted only from the address of one of its own
//! pods (T-4004): a private address alone proves nothing, because a cluster pod that dials the
//! public listener through Traefik arrives with its pod address in the PROXY header. The Portal's
//! capture subscriber is admitted the same way, from one of the Portal's pods, on a `capture`
//! broker only (ADR-N-048 §2.10).

use crate::app::Gateway;
use crate::auth::accounts::ServiceAccounts;
use crate::auth::token::{Rejected, Verifier};
use crate::mqtt_refusals::{account_of, Reader, Reason, Refusal, Refusals};
use axum::extract::State;
use axum::http::HeaderMap;
use axum::Json;
use jc_core::kinds::mqtt_broker::{
    release_name, runner_username, MqttMode, MqttRole, RUNNER_USERNAME,
};
use jc_core::kinds::{DataSourceSpec, MqttBrokerSpec};
use jc_manifest::loader::Repository;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;

/// The username the Portal's capture subscriber connects with on a `capture` broker's internal
/// listener (ADR-N-048 §2.10); the Portal's `mqtt_capture` module sends the same.
pub const CAPTURE_USERNAME: &str = "jc-portal-capture";

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
    /// The manifest's `limits.messagesPerSecond`: what one client may publish per second
    /// before [`Rates`] refuses it and the broker disconnects it (T-5562).
    pub messages_per_second: u32,
    /// What the Portal's capture subscriber may subscribe to: the publish filters of the roles,
    /// each `${clientid}` level widened to `+`; empty unless the broker is in `capture` mode.
    pub capture_topics: Vec<String>,
}

/// The filters a capture subscribes to: every role's publish filter, a level that names
/// `${clientid}` (or any other placeholder the broker binds) widened to `+`, since the
/// subscriber's own client id would bind it otherwise. Only devices publish, under their own
/// filters, so the widened filter reads nothing they could not write.
pub fn capture_topics(roles: &[MqttRole]) -> Vec<String> {
    let mut topics: Vec<String> = roles
        .iter()
        .flat_map(|role| &role.publish)
        .map(|filter| {
            filter
                .split('/')
                .map(|level| if level.contains("${") { "+" } else { level })
                .collect::<Vec<_>>()
                .join("/")
        })
        .collect();
    topics.sort();
    topics.dedup();
    topics
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
                        capture_topics: if spec.mode == Some(MqttMode::Capture) {
                            capture_topics(&spec.roles)
                        } else {
                            Vec::new()
                        },
                        roles: spec.roles,
                        runner_topics: Vec::new(),
                        messages_per_second: spec.limits.messages_per_second,
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

/// The publish rate of each client of each broker, one token bucket per (broker, client id) that
/// holds `messagesPerSecond` and refills that many per second (T-5562). RMQTT 0.24 limits only
/// what it delivers to a subscriber (`mqueue_rate_limit`), so the broker asks for each publish
/// and this refuses the one above the rate; the broker then disconnects the client.
#[derive(Debug, Default)]
pub struct Rates {
    buckets: std::sync::Mutex<HashMap<(String, String), Bucket>>,
}

#[derive(Debug, Clone, Copy)]
struct Bucket {
    tokens: f64,
    at: std::time::Instant,
}

/// Buckets kept before the idle ones go; only clients the publish check admitted get one.
const RATE_BUCKETS: usize = 4096;

impl Rates {
    /// Whether `clientid` of `broker` may publish one more message at `now`.
    pub fn admit(
        &self,
        broker: &str,
        clientid: &str,
        per_second: u32,
        now: std::time::Instant,
    ) -> bool {
        let capacity = f64::from(per_second.max(1));
        let mut buckets = self
            .buckets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if buckets.len() >= RATE_BUCKETS {
            // A bucket idle for a second is full again: dropping it changes no answer.
            buckets.retain(|_, b| now.saturating_duration_since(b.at).as_secs_f64() < 1.0);
        }
        let bucket = buckets
            .entry((broker.to_owned(), clientid.to_owned()))
            .or_insert(Bucket {
                tokens: capacity,
                at: now,
            });
        let refill = now.saturating_duration_since(bucket.at).as_secs_f64() * capacity;
        bucket.tokens = (bucket.tokens + refill).min(capacity);
        bucket.at = now;
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// RMQTT's answer format (`docs/en_US/auth-http.md`, `perm-list.md`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Answer {
    /// `allow` or `deny`; never `ignore`, which would hand the decision to the next plugin.
    pub result: &'static str,
    /// Always false: no client bypasses its ACL.
    pub superuser: bool,
    /// When the broker drops the connection: the token's `exp`, or the account's credential
    /// end when that comes first.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expire_at: Option<i64>,
    /// The client's subscriptions, fixed at CONNECT. An allow always carries the list, empty
    /// too: without one the broker forgets `expire_at`. A publish matches none of these rules,
    /// so the broker asks [`publish_check`] for each topic (T-5560).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub acl: Option<Vec<Rule>>,
}

/// One ACL rule of the answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Rule {
    /// Always `allow`: the broker's own last rule denies the rest.
    pub permission: &'static str,
    /// Always `subscribe`: a publish is checked per topic (T-5560).
    pub action: &'static str,
    /// A topic filter, `${clientid}` left for the broker to bind.
    pub topic: String,
    /// QoS 0 and 1; the listener refuses 2 anyway.
    pub qos: [u8; 2],
}

impl Answer {
    fn deny() -> Self {
        Self {
            result: "deny",
            superuser: false,
            expire_at: None,
            acl: None,
        }
    }

    fn allow(acl: Vec<Rule>, expire_at: Option<i64>) -> Self {
        Self {
            result: "allow",
            superuser: false,
            expire_at,
            acl: Some(acl),
        }
    }
}

fn rule(topic: &str) -> Rule {
    Rule {
        permission: "allow",
        action: "subscribe",
        topic: topic.to_owned(),
        qos: [0, 1],
    }
}

/// The broker a mesh identity names: Linkerd's `{serviceaccount}.{namespace}.serviceaccount.
/// identity.…` whose first label is the broker's release name.
pub fn broker_of(identity: &str) -> &str {
    identity.split('.').next().unwrap_or_default()
}

/// The addresses a tokenless CONNECT is admitted from: the pipeline runner's pods and the
/// Portal's, each resolved from its headless Service.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Pods {
    /// The pipeline runner's pods (PL-76).
    pub runner: Vec<IpAddr>,
    /// The Portal's pods, for its capture subscriber (ADR-N-048 §2.10).
    pub portal: Vec<IpAddr>,
}

/// What a CONNECT was decided on: the log's sentence and, for a refusal of a declared broker,
/// the reason the refusal record keeps (T-5534).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Why {
    /// The sentence the log line carries.
    pub text: &'static str,
    /// The reason a refusal of a declared broker is kept with; `None` for an admission or a
    /// caller that is no broker.
    pub refused: Option<Reason>,
}

impl Why {
    fn unrecorded(text: &'static str) -> Self {
        Self {
            text,
            refused: None,
        }
    }
}

fn refuse(text: &'static str, reason: Reason) -> (Answer, Why) {
    (
        Answer::deny(),
        Why {
            text,
            refused: Some(reason),
        },
    )
}

/// Decides one CONNECT (PL-75, PL-76, ADR-N-048 §2.10). `caller` is the mesh identity of the
/// broker asking; `pods` the addresses a tokenless CONNECT is admitted from.
pub fn decide(
    brokers: &Brokers,
    verifier: Option<&Verifier>,
    accounts: &ServiceAccounts,
    caller: Option<&str>,
    pods: &Pods,
    connect: &Connect,
) -> (Answer, Why) {
    let Some(release) = caller.map(broker_of) else {
        return (Answer::deny(), Why::unrecorded("no mesh identity"));
    };
    let Some(broker) = brokers.get(release) else {
        return (
            Answer::deny(),
            Why::unrecorded("the caller is no broker this repository declares"),
        );
    };
    if connect.password.is_empty() {
        return tokenless(broker, pods, connect);
    }
    let Some(verifier) = verifier else {
        return refuse("no realm configured", Reason::NoRealm);
    };
    let claims = match verifier.verify(&connect.password, &[release.to_owned()]) {
        Ok(claims) => claims,
        Err(Rejected::Expired) => return refuse("the token is expired", Reason::Expired),
        Err(Rejected::WrongAudience) => {
            return refuse(
                "the token is bound to another broker",
                Reason::WrongAudience,
            )
        }
        Err(_) => return refuse("the token does not verify", Reason::InvalidToken),
    };
    // The verifier's leeway is for clocks between services; the broker would only refuse a past
    // `expire_at` itself, so the gateway does it here without one.
    let now = chrono::Utc::now();
    if claims.exp <= now.timestamp() {
        return refuse("the token is expired", Reason::Expired);
    }
    let Some(azp) = claims.azp.as_deref() else {
        return refuse("the token names no client", Reason::WrongClient);
    };
    if azp != connect.username || azp != connect.clientid {
        return refuse(
            "the username or client id is not the token's client",
            Reason::WrongClient,
        );
    }
    match accounts.resolve(azp) {
        Some(account) => device(broker, account, claims.exp, now),
        None => refuse("the client is no ServiceAccount", Reason::UnknownAccount),
    }
}

/// Why `account` is no device of `broker` at `now`: another project's, or every credential ended.
fn refused(
    broker: &Broker,
    account: &crate::auth::accounts::Account,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<(&'static str, Reason)> {
    if account.project != broker.project {
        return Some((
            "the account belongs to another project",
            Reason::OtherProject,
        ));
    }
    account
        .credentials_end
        .is_some_and(|end| end <= now)
        .then_some(("the account's credentials have expired", Reason::Expired))
}

/// The CONNECT answer for a device whose token, valid until `exp`, named `account`.
fn device(
    broker: &Broker,
    account: &crate::auth::accounts::Account,
    exp: i64,
    now: chrono::DateTime<chrono::Utc>,
) -> (Answer, Why) {
    if let Some((why, reason)) = refused(broker, account, now) {
        return refuse(why, reason);
    }
    let roles = || {
        broker
            .roles
            .iter()
            .filter(|role| role.service_accounts.contains(&account.name))
    };
    if roles().next().is_none() {
        return refuse("the account holds no role of this broker", Reason::NoRole);
    }
    let acl = roles()
        .flat_map(|role| &role.subscribe)
        .map(|topic| rule(topic))
        .collect();
    let expire_at = account
        .credentials_end
        .map_or(exp, |end| exp.min(end.timestamp()));
    (
        Answer::allow(acl, Some(expire_at)),
        Why::unrecorded("a device"),
    )
}

/// What `rmqtt-auth-http` posts for a publish its CONNECT rules do not cover
/// (`http_acl_req.params`, JSON).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Access {
    /// `1` subscribe, `2` publish.
    #[serde(default)]
    pub access: String,
    /// The MQTT client id, the token's `azp` the CONNECT check held it to.
    #[serde(default)]
    pub clientid: String,
    /// The MQTT username, equal to the client id for a device.
    #[serde(default)]
    pub username: String,
    /// The topic of the publish.
    #[serde(default)]
    pub topic: String,
}

/// Whether the topic `topic` is under the filter `filter`, `${clientid}` standing for the level
/// `clientid` (MQTT 3.1.1 §4.7). A wildcard never reaches a `$` topic.
pub fn filter_matches(filter: &str, topic: &str, clientid: &str) -> bool {
    if topic.starts_with('$') || topic.contains(['+', '#']) {
        return false;
    }
    let mut topic = topic.split('/');
    for level in filter.split('/') {
        if level == "#" {
            return true;
        }
        let Some(part) = topic.next() else {
            return false;
        };
        let hit = match level {
            "+" => true,
            jc_core::kinds::mqtt_broker::CLIENT_ID => part == clientid,
            _ => part == level,
        };
        if !hit {
            return false;
        }
    }
    topic.next().is_none()
}

/// Decides one publish (T-5560) against the repository as it is now: the account must still
/// exist, still belong to the broker's project, still hold a role whose publish filter covers
/// the topic and still have a credential in force. The broker asks for every publish (no
/// `X-Cache`, T-5562) and disconnects a client whose publish is refused, so a device the
/// repository no longer admits stops at its next publish instead of at its token's `exp`.
pub fn publish_check(
    brokers: &Brokers,
    accounts: &ServiceAccounts,
    caller: Option<&str>,
    access: &Access,
) -> (bool, &'static str) {
    let Some(broker) = caller
        .map(broker_of)
        .and_then(|release| brokers.get(release))
    else {
        return (false, "the caller is no broker this repository declares");
    };
    if access.access != "2" {
        return (false, "a subscription not in the CONNECT answer");
    }
    // The tokenless subscribers never publish; their name is no device's even if an account
    // derived it.
    if access.username != access.clientid
        || [RUNNER_USERNAME, CAPTURE_USERNAME].contains(&access.username.as_str())
    {
        return (false, "no device");
    }
    let Some(account) = accounts.resolve(&access.clientid) else {
        return (false, "the client is no ServiceAccount");
    };
    if let Some((why, _)) = refused(broker, account, chrono::Utc::now()) {
        return (false, why);
    }
    let allowed = broker
        .roles
        .iter()
        .filter(|role| role.service_accounts.contains(&account.name))
        .flat_map(|role| &role.publish)
        .any(|filter| filter_matches(filter, &access.topic, &access.clientid));
    if allowed {
        (true, "a device's role")
    } else {
        (false, "no role of the account covers the topic")
    }
}

/// A CONNECT without a token: the runner or the Portal's capture subscriber, each by its name
/// from one of its own pods, with `subscribe` only and no `expire_at`. The runner's name carries
/// the broker's project, so a stream of another project is refused here (T-5561).
fn tokenless(broker: &Broker, pods: &Pods, connect: &Connect) -> (Answer, Why) {
    let from = |addresses: &[IpAddr]| {
        connect
            .ipaddr
            .parse::<IpAddr>()
            .is_ok_and(|ip| addresses.contains(&ip.to_canonical()))
    };
    let runner = runner_username(&broker.project);
    let (topics, who, none) = match connect.username.as_str() {
        name if name == runner && from(&pods.runner) => (
            &broker.runner_topics,
            "the pipeline runner",
            "no DataSource reads this broker",
        ),
        CAPTURE_USERNAME if from(&pods.portal) => (
            &broker.capture_topics,
            "the Portal's capture",
            "the broker is not in capture mode",
        ),
        _ => return refuse("no token", Reason::NoToken),
    };
    if topics.is_empty() {
        return refuse(none, Reason::NoSubscription);
    }
    let acl = topics.iter().map(|topic| rule(topic)).collect();
    (Answer::allow(acl, None), Why::unrecorded(who))
}

/// The addresses `host` resolves to: a headless Service lists one per pod. A name that does not
/// resolve admits nobody.
pub async fn runner_addresses(host: &str) -> Vec<IpAddr> {
    match tokio::net::lookup_host((host, 0)).await {
        Ok(found) => found.map(|address| address.ip().to_canonical()).collect(),
        Err(error) => {
            tracing::warn!(%error, host, "the pods admitted without a token do not resolve");
            Vec::new()
        }
    }
}

/// What the internal listener's routes share.
#[derive(Clone)]
struct Internal {
    gateway: Arc<Gateway>,
    runner_host: Option<Arc<str>>,
    portal_host: Option<Arc<str>>,
    refusals: Arc<Refusals>,
    rates: Arc<Rates>,
}

/// `POST /internal/mqtt/auth`, on the internal listener only. Always 200 with `allow` or `deny`:
/// a non-2xx would read as `ignore` to the broker (`deny_if_error` covers transport faults only).
async fn auth(
    State(Internal {
        gateway,
        runner_host,
        portal_host,
        refusals,
        ..
    }): State<Internal>,
    headers: HeaderMap,
    Json(connect): Json<Connect>,
) -> Json<Answer> {
    let caller = headers
        .get(MESH_IDENTITY)
        .and_then(|value| value.to_str().ok());
    let brokers = gateway.brokers();
    let accounts = gateway.service_accounts();
    // Resolved per tokenless CONNECT only: a subscriber reconnects rarely, a pod moves its address.
    let mut pods = Pods::default();
    if connect.password.is_empty() {
        let (host, addresses) = match connect.username.as_str() {
            name if name.starts_with(RUNNER_USERNAME) => (&runner_host, &mut pods.runner),
            CAPTURE_USERNAME => (&portal_host, &mut pods.portal),
            _ => (&None, &mut pods.runner),
        };
        if let Some(host) = host {
            *addresses = runner_addresses(host).await;
        }
    }
    let (answer, why) = decide(
        &brokers,
        gateway.verifier.as_deref(),
        &accounts,
        caller,
        &pods,
        &connect,
    );
    let release = caller.map(broker_of).unwrap_or_default();
    if let (Some(reason), Some(broker)) = (why.refused, brokers.get(release)) {
        refusals.record(
            release,
            Refusal {
                at: chrono::Utc::now(),
                reason,
                account: account_of(broker, &connect.username),
            },
        );
    }
    // The client id and the outcome, never the password.
    tracing::info!(
        broker = release,
        clientid = %connect.clientid,
        result = answer.result,
        why = why.text,
        "mqtt connect"
    );
    Json(answer)
}

/// `POST /internal/mqtt/acl`, on the internal listener only: 200 with `allow`, which the broker
/// keeps for no time, or `deny`, which disconnects the client: a publish the repository no longer
/// admits, or one above the broker's `messagesPerSecond` for that client (T-5562).
async fn acl(
    State(Internal { gateway, rates, .. }): State<Internal>,
    headers: HeaderMap,
    Json(access): Json<Access>,
) -> Json<serde_json::Value> {
    let caller = headers
        .get(MESH_IDENTITY)
        .and_then(|value| value.to_str().ok());
    let brokers = gateway.brokers();
    let (mut allowed, mut why) =
        publish_check(&brokers, &gateway.service_accounts(), caller, &access);
    if allowed {
        let release = caller.map(broker_of).unwrap_or_default();
        let per_second = brokers.get(release).map_or(1, |b| b.messages_per_second);
        if !rates.admit(
            release,
            &access.clientid,
            per_second,
            std::time::Instant::now(),
        ) {
            (allowed, why) = (
                false,
                "the client publishes above the broker's messagesPerSecond",
            );
        }
    }
    if !allowed {
        tracing::info!(
            broker = caller.map(broker_of).unwrap_or_default(),
            clientid = %access.clientid,
            why,
            "mqtt publish refused"
        );
    }
    let result = if allowed { "allow" } else { "deny" };
    Json(serde_json::json!({ "result": result }))
}

/// The internal listener's router: the CONNECT check, the publish check and the record of the
/// CONNECT refusals. `runner_host` and `portal_host` name the headless Services of the pipeline
/// runner and of the Portal; without one, nobody connects under that name without a token.
/// `portal_identity` is the Portal's `{serviceaccount}.{namespace}`, the only mesh identity that
/// reads the refusals; without it nobody does (T-5534).
pub fn internal_router(
    gateway: Arc<Gateway>,
    runner_host: Option<String>,
    portal_host: Option<String>,
    portal_identity: Option<String>,
) -> axum::Router {
    let refusals = Arc::new(Refusals::default());
    let reader = Reader {
        refusals: Arc::clone(&refusals),
        portal_identity: portal_identity.map(Arc::from),
    };
    axum::Router::new()
        .route("/internal/mqtt/auth", axum::routing::post(auth))
        .route("/internal/mqtt/acl", axum::routing::post(acl))
        .with_state(Internal {
            gateway,
            runner_host: runner_host.map(Arc::from),
            portal_host: portal_host.map(Arc::from),
            refusals,
            rates: Arc::default(),
        })
        .merge(
            axum::Router::new()
                .route(
                    "/internal/mqtt/refusals/{broker}",
                    axum::routing::get(crate::mqtt_refusals::read),
                )
                .with_state(reader),
        )
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
    fn a_capture_reads_the_publish_filters_with_each_placeholder_level_widened() {
        let role = |publish: &[&str]| MqttRole {
            name: "r".into(),
            service_accounts: vec!["a".into()],
            publish: publish.iter().map(|t| (*t).to_owned()).collect(),
            subscribe: vec!["commands/${clientid}".into()],
            shared_topics: false,
        };
        assert_eq!(
            capture_topics(&[
                role(&["cams/${clientid}/frame", "cams/dev-${clientid}/#"]),
                role(&["status", "cams/+/frame"]),
            ]),
            ["cams/+/#", "cams/+/frame", "status"]
        );
        assert!(capture_topics(&[role(&[])]).is_empty());
    }

    #[test]
    fn a_publish_is_refused_for_a_subscription_a_tokenless_name_or_an_unknown_client() {
        let brokers = Brokers::from([(
            "mqtt-p-b".to_owned(),
            Broker {
                project: "p".into(),
                roles: Vec::new(),
                runner_topics: Vec::new(),
                capture_topics: Vec::new(),
                messages_per_second: 10,
            },
        )]);
        let accounts = ServiceAccounts::new();
        let ask = |caller, access: &str, clientid: &str| {
            let access = Access {
                access: access.into(),
                clientid: clientid.into(),
                username: clientid.into(),
                topic: "t".into(),
            };
            publish_check(&brokers, &accounts, caller, &access).1
        };
        assert_eq!(
            ask(Some("mqtt-q-b.ns"), "2", "p-cam"),
            "the caller is no broker this repository declares"
        );
        assert_eq!(
            ask(Some("mqtt-p-b.ns"), "1", "p-cam"),
            "a subscription not in the CONNECT answer"
        );
        assert_eq!(ask(Some("mqtt-p-b.ns"), "2", CAPTURE_USERNAME), "no device");
        assert_eq!(
            ask(Some("mqtt-p-b.ns"), "2", "p-cam"),
            "the client is no ServiceAccount"
        );
    }

    #[test]
    fn a_filter_covers_a_topic_level_by_level() {
        for (filter, topic) in [
            ("cameras/${clientid}/#", "cameras/cam-1/frame/0"),
            ("cameras/${clientid}/#", "cameras/cam-1"),
            ("air/+/pm", "air/x/pm"),
            ("air/status", "air/status"),
        ] {
            assert!(filter_matches(filter, topic, "cam-1"), "{filter} {topic}");
        }
        for (filter, topic) in [
            ("cameras/${clientid}/#", "cameras/cam-2/frame"),
            ("cameras/${clientid}/#", "cameras"),
            ("air/+/pm", "air/x/pm/y"),
            ("air/+/pm", "air/pm"),
            ("air/status", "air/status/x"),
            ("+/status", "$SYS/status"),
            ("air/+", "air/#"),
        ] {
            assert!(!filter_matches(filter, topic, "cam-1"), "{filter} {topic}");
        }
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
        let answer = Answer::allow(vec![rule("x/${clientid}")], Some(1_900_000_000));
        assert_eq!(
            serde_json::to_value(&answer).expect("serializes"),
            serde_json::json!({
                "result": "allow", "superuser": false, "expire_at": 1_900_000_000,
                "acl": [
                    {"permission": "allow", "action": "subscribe", "topic": "x/${clientid}", "qos": [0, 1]}
                ]
            })
        );
        assert_eq!(
            serde_json::to_value(Answer::deny()).expect("serializes"),
            serde_json::json!({"result": "deny", "superuser": false})
        );
    }

    #[test]
    fn a_client_above_its_rate_is_refused_until_its_bucket_refills() {
        let rates = Rates::default();
        let start = std::time::Instant::now();
        let at = |ms| start + std::time::Duration::from_millis(ms);
        // A burst of messagesPerSecond passes, the next one does not.
        assert!((0..5).all(|_| rates.admit("mqtt-p-b", "p-cam", 5, at(0))));
        assert!(!rates.admit("mqtt-p-b", "p-cam", 5, at(0)));
        // Another client and another broker keep their own buckets.
        assert!(rates.admit("mqtt-p-b", "p-other", 5, at(0)));
        assert!(rates.admit("mqtt-q-b", "p-cam", 5, at(0)));
        // 200 ms refill one message at 5 per second, and never more than the burst.
        assert!(rates.admit("mqtt-p-b", "p-cam", 5, at(200)));
        assert!(!rates.admit("mqtt-p-b", "p-cam", 5, at(200)));
        assert_eq!(
            (0..10)
                .filter(|_| rates.admit("mqtt-p-b", "p-cam", 5, at(60_000)))
                .count(),
            5
        );
    }
}
