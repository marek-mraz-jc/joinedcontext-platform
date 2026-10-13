//! `{endpoint}/ws` is served by the Endpoint's fan-out (EP-106, EP-107, EP-114, ADR-N-049): one
//! broker subscription per Endpoint, its changes cut to each caller's grant and matched to each
//! client's filters in the gateway. The gateway serves on a real socket, because an upgrade needs
//! one, in front of a stub broker that records each upgrade it is asked for, welcomes the
//! gateway, accepts its one subscription and sends whatever notification a test pushes.

mod common;

use axum::extract::ws::{Message as AxumMessage, WebSocketUpgrade};
use axum::extract::Path;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::get;
use axum::Router;
use context_gateway::app::{router, Gateway};
use context_gateway::auth::accounts::ServiceAccounts;
use context_gateway::handlers::ws::PER_CALLER;
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::store;
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::{Error, Message};

const HOST: &str = "https://hel.example.fi";
/// A public Endpoint that lists nothing, so it serves `websocket` (EP-104).
const PUBLIC: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaa";
/// An Endpoint for its own project.
const INTERNAL: &str = "cccccccccccccccccccccccccc";
/// A public Endpoint that turned the WebSocket off.
const DISABLED: &str = "dddddddddddddddddddddddddd";

const SPACE: &str = "apiVersion: joinedcontext.com/v1alpha1\nkind: ContextSpace\nmetadata:\n  \
                     name: helsinki\n  namespace: helsinki\nspec:\n  isSandbox: false\n  \
                     urnSegment: helsinki\n";

fn endpoint(name: &str, slug: &str, audience: &str, extra: &str) -> String {
    format!(
        "apiVersion: joinedcontext.com/v1alpha1\nkind: Endpoint\nmetadata:\n  name: {name}\n  \
         namespace: helsinki\nspec:\n  contextSpaceRef: helsinki\n  slug: {slug}\n  \
         audience: {audience}\n{extra}"
    )
}

fn policy(name: &str, role: &str) -> String {
    format!(
        "apiVersion: joinedcontext.com/v1alpha1\nkind: Policy\nmetadata:\n  name: {name}\n  \
         namespace: helsinki\nspec:\n  contextSpaceRef: {{ kind: ContextSpace, name: helsinki }}\n  \
         assigner: did:web:hel.fi\n  assignee: {{ kind: role, id: {role} }}\n  \
         operations: [retrieveOps]\n  information:\n    - entities:\n        - type: BikeHireDockingStation\n"
    )
}

fn repo(test: &str) -> PathBuf {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after the epoch")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("gw-ws-{test}-{now}"));
    std::fs::create_dir_all(&dir).expect("create the repository");
    let write = |name: &str, body: &str| std::fs::write(dir.join(name), body).expect("write");
    write("space.yaml", SPACE);
    write(
        "public.yaml",
        &endpoint("helsinki-bikes", PUBLIC, "public", ""),
    );
    write(
        "internal.yaml",
        &endpoint(
            "helsinki-ops",
            INTERNAL,
            "project-list",
            "  allowedProjects: [hel-mobility]\n",
        ),
    );
    write(
        "disabled.yaml",
        &endpoint(
            "helsinki-quiet",
            DISABLED,
            "public",
            "  disabledRepresentations: [websocket]\n",
        ),
    );
    write("public-policy.yaml", &policy("public-bikes", "public"));
    write("staff.yaml", &policy("staff-bikes", "data-steward"));
    dir
}

type Upgrades = Arc<Mutex<Vec<HeaderMap>>>;

/// The stub broker: the view upgrades it was asked for, and the notifications a test pushes to
/// every upstream it holds.
#[derive(Clone)]
struct Stub {
    upgrades: Upgrades,
    push: tokio::sync::broadcast::Sender<String>,
}

impl Stub {
    /// Sends `entities` to every open upstream as one `x-notification`.
    fn notify(&self, entities: serde_json::Value) {
        let message = json!({"metadata": {"x-op": "x-notification", "x-seq": 1},
            "body": {"type": "Notification", "data": entities}});
        let _ = self.push.send(message.to_string());
    }

    /// Every upstream goes away without a close frame.
    fn drop_upstreams(&self) {
        let _ = self.push.send("drop".to_owned());
    }

    /// The view upgrades so far.
    fn upgrades(&self) -> Vec<HeaderMap> {
        self.upgrades.lock().expect("the upgrade log").clone()
    }
}

/// A broker whose view WebSocket welcomes, answers `x-subscribe` with `201` and forwards what
/// [`Stub::notify`] pushes; its entity query answers an empty page.
async fn broker() -> (String, Stub) {
    broker_answering(201).await
}

/// [`broker`] answering the gateway's `x-subscribe` with `status`.
async fn broker_answering(status: u16) -> (String, Stub) {
    let (push, _) = tokio::sync::broadcast::channel(16);
    let stub = Stub {
        upgrades: Arc::default(),
        push,
    };
    let held = stub.clone();
    let app = Router::new()
        .route(
            "/x/views/{view}/ws",
            get(
                move |Path(_view): Path<String>, headers: HeaderMap, upgrade: WebSocketUpgrade| {
                    let stub = held.clone();
                    async move {
                        stub.upgrades.lock().expect("the upgrade log").push(headers);
                        let mut pushed = stub.push.subscribe();
                        upgrade.on_upgrade(move |mut socket| async move {
                            let welcome = json!({"metadata": {"x-op": "x-welcome",
                                "x-connection": "c1", "x-resume-token": "t1"}});
                            if socket.send(AxumMessage::Text(welcome.to_string().into())).await.is_err() {
                                return;
                            }
                            loop {
                                tokio::select! {
                                    read = socket.recv() => {
                                        let Some(Ok(AxumMessage::Text(text))) = read else { return };
                                        // It holds no place for a resume: subscribe again.
                                        if text.as_str().contains("x-resume") {
                                            let gone = json!({"metadata": {"x-op": "x-result",
                                                "x-id": "resume", "x-status": 404}});
                                            if socket.send(AxumMessage::Text(gone.to_string().into())).await.is_err() {
                                                return;
                                            }
                                        }
                                        if text.as_str().contains("x-subscribe") {
                                            let created = if status == 201 {
                                                json!({"metadata": {"x-op": "x-result", "x-id": "up", "x-status": 201},
                                                    "body": {"id": "urn:ngsi-ld:Subscription:up", "type": "Subscription"}})
                                            } else {
                                                json!({"metadata": {"x-op": "x-result", "x-id": "up", "x-status": status},
                                                    "body": {"title": "Forbidden", "status": status,
                                                             "detail": "no policy grants this operation"}})
                                            };
                                            if socket.send(AxumMessage::Text(created.to_string().into())).await.is_err() {
                                                return;
                                            }
                                        }
                                    }
                                    Ok(text) = pushed.recv() => {
                                        // `drop`: the broker goes away without a close frame.
                                        if text == "drop" {
                                            return;
                                        }
                                        if socket.send(AxumMessage::Text(text.into())).await.is_err() {
                                            return;
                                        }
                                    }
                                }
                            }
                        })
                    }
                },
            ),
        )
        .route(
            "/x/views/{view}/ngsi-ld/v1/entities",
            get(|| async { axum::Json(json!([])) }),
        );
    (serve(app).await, stub)
}

/// The next text message of `socket` whose `x-op` is `op`, skipping the others; `None` when the
/// socket ends or nothing arrives within two seconds.
async fn next_op(socket: &mut Socket, op: &str) -> Option<serde_json::Value> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        let read = tokio::time::timeout_at(deadline, socket.next())
            .await
            .ok()??;
        if let Ok(Message::Text(text)) = read {
            let message: serde_json::Value = serde_json::from_str(text.as_str()).ok()?;
            if message["metadata"]["x-op"] == op {
                return Some(message);
            }
        }
    }
}

/// Waits until the stub broker was asked for `n` upstreams and holds one open.
async fn upstreams(stub: &Stub, n: usize) {
    for _ in 0..250 {
        if stub.upgrades().len() >= n && stub.push.receiver_count() >= 1 {
            // The subscription's `x-result` is on its way; give the gateway a moment to read it.
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("the gateway opened no upstream");
}

fn dock(id: &str, bikes: u32) -> serde_json::Value {
    json!({"id": id, "type": "BikeHireDockingStation",
           "availableBikeNumber": {"type": "Property", "value": bikes}})
}

async fn serve(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a free port");
    let address = listener.local_addr().expect("an address");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("127.0.0.1:{}", address.port())
}

/// The gateway on a socket of its own, in front of `upstream`.
async fn gateway(test: &str, upstream: &str, realm: &common::Realm) -> String {
    lived(test, upstream, realm, Duration::from_secs(300)).await
}

/// [`gateway`] with a token caller's socket ending after `lifetime`.
async fn lived(test: &str, upstream: &str, realm: &common::Realm, lifetime: Duration) -> String {
    let (endpoints, ..) = store::load(&repo(test)).expect("the repository loads");
    let mut gateway = Gateway::new(
        Broker::new(format!("http://{upstream}")),
        Box::new(PolicyPdp),
        "hel.fi",
    )
    .serve(endpoints)
    .authenticate(
        Arc::new(realm.verifier()),
        ServiceAccounts::new(),
        Some(HOST.to_owned()),
    );
    gateway.ws_token_lifetime = lifetime;
    serve(router(Arc::new(gateway))).await
}

/// A person of `project`, holding `data-steward`, with a token for `slug`.
fn person(realm: &common::Realm, project: &str, slug: &str) -> String {
    realm.mint(&json!({
        "iss": common::ISSUER,
        "sub": format!("someone-of-{project}"),
        "aud": slug,
        "preferred_username": format!("someone-of-{project}"),
        "groups": [format!("/{project}")],
        "realm_access": { "roles": ["data-steward"] },
        "exp": common::in_seconds(300),
        "iat": common::in_seconds(-10),
    }))
}

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Opens `{slug}/ws{query}` with `headers`; the HTTP status of a refused upgrade otherwise.
async fn open(
    at: &str,
    slug: &str,
    query: &str,
    headers: &[(&str, String)],
) -> Result<(Socket, HeaderMap), StatusCode> {
    let mut request = format!("ws://{at}/api/endpoint/{slug}/ws{query}")
        .into_client_request()
        .expect("a request");
    for (name, value) in headers {
        request.headers_mut().insert(
            axum::http::HeaderName::from_bytes(name.as_bytes()).expect("a name"),
            value.parse().expect("a value"),
        );
    }
    match tokio_tungstenite::connect_async(request).await {
        Ok((socket, answer)) => Ok((socket, answer.headers().clone())),
        Err(Error::Http(answer)) => Err(answer.status()),
        Err(error) => panic!("the upgrade failed below HTTP: {error}"),
    }
}

/// EP-106: no token on an Endpoint that is not public is `401`, never a `101`, and the broker
/// is not asked.
#[tokio::test]
async fn no_token_is_401_before_any_upgrade() {
    let realm = common::Realm::new();
    let (upstream, stub) = broker().await;
    let at = gateway("anonymous", &upstream, &realm).await;
    assert_eq!(
        open(&at, INTERNAL, "", &[]).await.err(),
        Some(StatusCode::UNAUTHORIZED)
    );
    assert!(stub.upgrades().is_empty());
}

/// A stranger's token gets on the WebSocket what it gets on REST.
#[tokio::test]
async fn a_strangers_token_is_refused_as_on_rest() {
    let realm = common::Realm::new();
    let (upstream, stub) = broker().await;
    let at = gateway("stranger", &upstream, &realm).await;
    let stranger = person(&realm, "doprava", INTERNAL);
    let refused = open(
        &at,
        INTERNAL,
        "",
        &[("authorization", format!("Bearer {stranger}"))],
    )
    .await
    .expect_err("refused");
    let rest = reqwest::Client::new()
        .get(format!(
            "http://{at}/api/endpoint/{INTERNAL}/ngsi-ld/v1/entities?type=BikeHireDockingStation"
        ))
        .bearer_auth(&stranger)
        .send()
        .await
        .expect("REST answers");
    assert_eq!(refused.as_u16(), rest.status().as_u16());
    assert!(refused.is_client_error());
    assert!(stub.upgrades().is_empty());
}

/// EP-107: a token in the URL is refused and never read, even a valid one.
#[tokio::test]
async fn a_token_in_the_query_is_refused() {
    let realm = common::Realm::new();
    let (upstream, stub) = broker().await;
    let at = gateway("query", &upstream, &realm).await;
    let token = person(&realm, "hel-mobility", INTERNAL);
    assert_eq!(
        open(&at, INTERNAL, &format!("?access_token={token}"), &[])
            .await
            .err(),
        Some(StatusCode::BAD_REQUEST)
    );
    assert!(stub.upgrades().is_empty());
}

/// EP-104: an Endpoint that turned the WebSocket off answers like any surface it does not serve.
#[tokio::test]
async fn a_disabled_websocket_is_404() {
    let realm = common::Realm::new();
    let (upstream, _) = broker().await;
    let at = gateway("disabled", &upstream, &realm).await;
    assert_eq!(
        open(&at, DISABLED, "", &[]).await.err(),
        Some(StatusCode::NOT_FOUND)
    );
}

/// EP-106, EP-114: a browser's token rides as a subprotocol entry and the answer names only
/// `ngsi-ld-ws`. The broker sees one upstream on the gateway's own account, never the caller or
/// the token, and the caller receives exactly the change its grant and its filter allow.
#[tokio::test]
async fn a_subscriber_receives_what_its_grant_and_filter_allow() {
    let realm = common::Realm::new();
    let (upstream, stub) = broker().await;
    let at = gateway("fanout", &upstream, &realm).await;
    let token = person(&realm, "hel-mobility", INTERNAL);
    let (mut socket, answer) = open(
        &at,
        INTERNAL,
        "",
        &[
            (
                "sec-websocket-protocol",
                format!("ngsi-ld-ws, bearer.{token}"),
            ),
            ("origin", HOST.to_owned()),
        ],
    )
    .await
    .expect("upgraded");
    assert_eq!(answer["sec-websocket-protocol"], "ngsi-ld-ws");
    let welcome = next_op(&mut socket, "x-welcome").await.expect("welcomed");
    assert!(welcome["metadata"]["x-resume-token"].is_string());
    let subscribe = json!({"metadata": {"x-op": "x-subscribe", "x-id": "s1"},
        "body": {"type": "Subscription", "entities": [{"type": "BikeHireDockingStation"}],
                 "q": "availableBikeNumber>3"}});
    socket
        .send(Message::text(subscribe.to_string()))
        .await
        .expect("sent");
    let created = next_op(&mut socket, "x-result").await.expect("answered");
    assert_eq!(created["metadata"]["x-id"], "s1");
    assert_eq!(created["metadata"]["x-status"], 201);
    upstreams(&stub, 1).await;

    stub.notify(json!([
        dock("urn:ngsi-ld:BikeHireDockingStation:few", 1),
        dock("urn:ngsi-ld:BikeHireDockingStation:many", 9),
        {"id": "urn:ngsi-ld:Vehicle:v1", "type": "Vehicle", "speed": {"type": "Property", "value": 9}},
    ]));
    let note = next_op(&mut socket, "x-notification")
        .await
        .expect("notified");
    assert_eq!(note["metadata"]["x-seq"], 1);
    assert_eq!(
        note["body"]["data"][0]["id"],
        "urn:ngsi-ld:BikeHireDockingStation:many"
    );
    assert!(
        next_op(&mut socket, "x-notification").await.is_none(),
        "nothing outside the filter or the grant"
    );

    let upgrade = stub.upgrades()[0].clone();
    assert_eq!(upgrade["ngsild-tenant"], "helsinki");
    assert_eq!(upgrade["x-jc-client"], "context-gateway");
    assert!(!upgrade.contains_key("x-jc-subject"));
    assert!(!upgrade.contains_key("authorization"));
    assert!(!upgrade.contains_key("origin"));
    assert!(
        !format!("{upgrade:?}").contains(&token),
        "the token never reaches the broker"
    );
}

/// ADR-N-049: however many clients an Endpoint has, the broker holds one upstream for it.
#[tokio::test]
async fn every_client_of_an_endpoint_shares_one_upstream() {
    let realm = common::Realm::new();
    let (upstream, stub) = broker().await;
    let at = gateway("shared", &upstream, &realm).await;
    let mut sockets = Vec::new();
    for n in 0..3 {
        let address = [("x-forwarded-for", format!("203.0.113.{n}"))];
        let (mut socket, _) = open(&at, PUBLIC, "", &address).await.expect("upgraded");
        next_op(&mut socket, "x-welcome").await.expect("welcomed");
        sockets.push(socket);
    }
    upstreams(&stub, 1).await;
    assert_eq!(stub.upgrades().len(), 1, "one upstream for three clients");
}

/// The broker's own `Origin` rule refuses every browser, so the gateway judges it: a page on
/// another site is refused before the broker is asked.
#[tokio::test]
async fn a_foreign_origin_is_refused() {
    let realm = common::Realm::new();
    let (upstream, stub) = broker().await;
    let at = gateway("origin", &upstream, &realm).await;
    assert_eq!(
        open(
            &at,
            PUBLIC,
            "",
            &[("origin", "https://evil.example".to_owned())]
        )
        .await
        .err(),
        Some(StatusCode::FORBIDDEN)
    );
    assert!(stub.upgrades().is_empty());
}

/// EP-107: a caller holds at most its cap; one more is `429` before an upgrade, and a closed
/// connection gives its place back.
#[tokio::test]
async fn connections_are_capped_per_caller() {
    let realm = common::Realm::new();
    let (upstream, _) = broker().await;
    let at = gateway("cap", &upstream, &realm).await;
    let address = [("x-forwarded-for", "203.0.113.7".to_owned())];
    let mut held = Vec::new();
    for _ in 0..PER_CALLER {
        held.push(
            open(&at, PUBLIC, "", &address)
                .await
                .expect("under the cap")
                .0,
        );
    }
    assert_eq!(
        open(&at, PUBLIC, "", &address).await.err(),
        Some(StatusCode::TOO_MANY_REQUESTS)
    );
    let other = [("x-forwarded-for", "203.0.113.8".to_owned())];
    assert!(
        open(&at, PUBLIC, "", &other).await.is_ok(),
        "another caller"
    );
    let mut first = held.remove(0);
    first.close(None).await.expect("closed");
    drop(first);
    let mut again = None;
    for _ in 0..50 {
        if let Ok(socket) = open(&at, PUBLIC, "", &address).await {
            again = Some(socket);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(again.is_some(), "the closed connection gave its place back");
}

/// A message over the bound closes the connection with 1009, and it never reaches the broker.
#[tokio::test]
async fn an_oversized_message_closes_with_1009() {
    let realm = common::Realm::new();
    let (upstream, _) = broker().await;
    let at = gateway("big", &upstream, &realm).await;
    let (mut socket, _) = open(&at, PUBLIC, "", &[]).await.expect("upgraded");
    socket
        .send(Message::text("x".repeat(70 * 1024)))
        .await
        .expect("sent");
    let mut code = None;
    while let Some(Ok(message)) = socket.next().await {
        if let Message::Close(frame) = message {
            code = frame.map(|frame| u16::from(frame.code));
            break;
        }
        if let Message::Text(text) = &message {
            assert!(
                text.as_str().contains("x-welcome"),
                "only the welcome: {text}"
            );
        }
    }
    assert_eq!(code, Some(1009));
}

/// T-4064: a plain GET (a browser tab, curl) is no upgrade. It is told how to use the address,
/// `426` with the subprotocol, the token rule, one `x-subscribe` message and the guide, never
/// axum's bare `400`; and the broker is not asked.
#[tokio::test]
async fn a_plain_get_is_told_how_to_use_the_websocket() {
    let realm = common::Realm::new();
    let (upstream, stub) = broker().await;
    let at = gateway("guide", &upstream, &realm).await;
    let client = reqwest::Client::new();
    let url = format!("http://{at}/api/endpoint/{PUBLIC}/ws");
    let guide = format!("{HOST}/docs/API/07-websocket");

    let answer = client.get(&url).send().await.expect("answered");
    assert_eq!(answer.status(), StatusCode::UPGRADE_REQUIRED);
    assert_eq!(answer.headers()["upgrade"], "websocket");
    assert_eq!(answer.headers()["link"], format!("<{guide}>; rel=\"help\""));
    assert!(answer.headers()["content-type"]
        .to_str()
        .expect("ascii")
        .starts_with("application/problem+json"));
    let body: serde_json::Value = answer.json().await.expect("a problem");
    assert!(body["type"]
        .as_str()
        .is_some_and(|t| t.ends_with("upgrade-required")));
    assert_eq!(body["protocol"], "ngsi-ld-ws");
    assert_eq!(
        body["address"],
        format!("wss://hel.example.fi/api/endpoint/{PUBLIC}/ws")
    );
    assert!(body["token"]
        .as_str()
        .is_some_and(|t| t.starts_with("none")));
    assert_eq!(body["example"]["metadata"]["x-op"], "x-subscribe");
    assert_eq!(
        body["example"]["body"]["entities"][0]["type"],
        "BikeHireDockingStation"
    );
    let websocat = body["websocat"].as_str().expect("a websocat line");
    assert!(websocat.contains("--protocol ngsi-ld-ws") && !websocat.contains("Authorization"));
    assert_eq!(body["guide"], guide);

    let page = client
        .get(&url)
        .header("accept", "text/html,application/xhtml+xml")
        .send()
        .await
        .expect("answered");
    assert_eq!(page.status(), StatusCode::UPGRADE_REQUIRED);
    assert!(page.headers()["content-type"]
        .to_str()
        .expect("ascii")
        .starts_with("text/html"));
    let html = page.text().await.expect("a page");
    assert!(html.contains("<code>ngsi-ld-ws</code>"), "{html}");
    assert!(html.contains("x-subscribe") && html.contains(&format!("href=\"{guide}\"")));

    assert!(stub.upgrades().is_empty(), "no upgrade reached the broker");
}

/// T-4064: on an Endpoint that is not public, the plain GET is still admitted first (no token
/// is `401`), and the guide names the token the upgrade needs.
#[tokio::test]
async fn a_plain_get_of_a_restricted_websocket_names_the_token() {
    let realm = common::Realm::new();
    let (upstream, _) = broker().await;
    let at = gateway("guide-restricted", &upstream, &realm).await;
    let client = reqwest::Client::new();
    let url = format!("http://{at}/api/endpoint/{INTERNAL}/ws");
    assert_eq!(
        client.get(&url).send().await.expect("answered").status(),
        StatusCode::UNAUTHORIZED
    );
    let token = person(&realm, "hel-mobility", INTERNAL);
    let answer = client
        .get(&url)
        .bearer_auth(&token)
        .send()
        .await
        .expect("answered");
    assert_eq!(answer.status(), StatusCode::UPGRADE_REQUIRED);
    let body: serde_json::Value = answer.json().await.expect("a problem");
    assert!(body["websocat"]
        .as_str()
        .is_some_and(|line| line.contains("Authorization: Bearer $TOKEN")));
    assert!(
        !body.to_string().contains(&token),
        "the token is never echoed"
    );
}

/// Subscribes `socket` to every BikeHireDockingStation and waits for the `201`.
async fn subscribe_docks(socket: &mut Socket) {
    let subscribe = json!({"metadata": {"x-op": "x-subscribe", "x-id": "s1"},
        "body": {"type": "Subscription", "entities": [{"type": "BikeHireDockingStation"}]}});
    socket
        .send(Message::text(subscribe.to_string()))
        .await
        .expect("sent");
    let created = next_op(socket, "x-result").await.expect("answered");
    assert_eq!(created["metadata"]["x-status"], 201, "{created}");
}

/// T-4131: the broker writes notifications of up to 1 MiB in one frame; one over the 16 KiB a
/// client may send reaches the client whole, and the connection stays up.
#[tokio::test]
async fn a_notification_over_the_client_bound_reaches_the_client() {
    let realm = common::Realm::new();
    let (upstream, stub) = broker().await;
    let at = gateway("big-notification", &upstream, &realm).await;
    let (mut socket, _) = open(&at, PUBLIC, "", &[]).await.expect("upgraded");
    subscribe_docks(&mut socket).await;
    upstreams(&stub, 1).await;

    let mut big = dock("urn:ngsi-ld:BikeHireDockingStation:big", 1);
    big["note"] = json!({"type": "Property", "value": "n".repeat(200_000)});
    stub.notify(json!([big]));
    let note = next_op(&mut socket, "x-notification")
        .await
        .expect("the big one");
    assert_eq!(
        note["body"]["data"][0]["note"]["value"]
            .as_str()
            .map(str::len),
        Some(200_000)
    );

    stub.notify(json!([dock("urn:ngsi-ld:BikeHireDockingStation:big", 2)]));
    let after = next_op(&mut socket, "x-notification")
        .await
        .expect("still up");
    assert_eq!(after["metadata"]["x-seq"], 2);
}

/// T-4131, ADR-N-049: when the broker's side ends without a close frame, the client stays: the
/// upstream comes back, tells the client with `x-gap` that it may have missed something, and
/// changes reach it again.
#[tokio::test]
async fn a_broker_gone_mid_stream_leaves_the_client_up() {
    let realm = common::Realm::new();
    let (upstream, stub) = broker().await;
    let at = gateway("broker-gone", &upstream, &realm).await;
    let (mut socket, _) = open(&at, PUBLIC, "", &[]).await.expect("upgraded");
    subscribe_docks(&mut socket).await;
    upstreams(&stub, 1).await;

    stub.drop_upstreams();
    let gap = next_op(&mut socket, "x-gap")
        .await
        .expect("told of the gap");
    assert!(gap["metadata"]["x-from"].is_u64());
    upstreams(&stub, 2).await;
    stub.notify(json!([dock("urn:ngsi-ld:BikeHireDockingStation:back", 4)]));
    let note = next_op(&mut socket, "x-notification")
        .await
        .expect("changes flow again");
    assert_eq!(
        note["body"]["data"][0]["id"],
        "urn:ngsi-ld:BikeHireDockingStation:back"
    );
}

/// T-4132: a token caller's socket ends at its lifetime with `x-closing: lifetime` and a normal
/// close; an anonymous socket on a public Endpoint outlives it.
#[tokio::test]
async fn a_token_socket_ends_at_its_lifetime_and_an_anonymous_one_does_not() {
    let realm = common::Realm::new();
    let (upstream, _) = broker().await;
    let at = lived("lifetime", &upstream, &realm, Duration::from_millis(300)).await;
    let token = person(&realm, "hel-mobility", INTERNAL);
    let (mut held, _) = open(
        &at,
        INTERNAL,
        "",
        &[("authorization", format!("Bearer {token}"))],
    )
    .await
    .expect("upgraded");
    let (mut anonymous, _) = open(&at, PUBLIC, "", &[]).await.expect("upgraded");

    let closing = next_op(&mut held, "x-closing").await.expect("told why");
    assert_eq!(closing["metadata"]["x-reason"], "lifetime");
    match held.next().await.expect("a frame").expect("readable") {
        Message::Close(Some(frame)) => assert_eq!(u16::from(frame.code), 1000),
        other => panic!("a close frame, not {other:?}"),
    }

    tokio::time::sleep(Duration::from_millis(200)).await;
    subscribe_docks(&mut anonymous).await;
}

/// T-4133: when the broker refuses the Endpoint's upstream subscription, no client is left
/// with a silent `201`: a connection that subscribed ends with 1011, and a later `x-subscribe`
/// is answered 503 naming the broker's refusal.
#[tokio::test]
async fn a_refused_upstream_is_an_error_for_every_client() {
    let realm = common::Realm::new();
    let (upstream, stub) = broker_answering(403).await;
    let at = gateway("refused", &upstream, &realm).await;
    let (mut first, _) = open(&at, PUBLIC, "", &[]).await.expect("upgraded");
    let subscribe = json!({"metadata": {"x-op": "x-subscribe", "x-id": "s1"},
        "body": {"type": "Subscription", "entities": [{"type": "BikeHireDockingStation"}]}});
    first
        .send(Message::text(subscribe.to_string()))
        .await
        .expect("sent");
    upstreams(&stub, 1).await;
    let closed = tokio::time::timeout(Duration::from_secs(2), async {
        while let Some(Ok(frame)) = first.next().await {
            if let Message::Close(frame) = frame {
                return frame;
            }
        }
        None
    })
    .await
    .expect("closed in time")
    .expect("a close frame");
    assert_eq!(u16::from(closed.code), 1011, "{closed:?}");

    let (mut later, _) = open(&at, PUBLIC, "", &[]).await.expect("upgraded");
    later
        .send(Message::text(subscribe.to_string()))
        .await
        .expect("sent");
    let refused = next_op(&mut later, "x-result").await.expect("answered");
    assert_eq!(refused["metadata"]["x-status"], 503, "{refused}");
    let detail = refused["body"]["detail"].as_str().unwrap_or_default();
    assert!(detail.contains("403"), "{detail}");
}
