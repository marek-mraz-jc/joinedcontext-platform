//! `{endpoint}/ws` is the broker view's WebSocket, relayed under the verified caller (EP-106,
//! EP-107, T-3885). The gateway serves on a real socket, because an upgrade needs one, in front
//! of a stub broker that records each upgrade it is asked for and echoes every frame; what the
//! broker does with a subscription is Antares' (ADR-0026) and tested there.

mod common;

use axum::extract::ws::{Message as AxumMessage, WebSocketUpgrade};
use axum::extract::Path;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
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
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::{Error, Message};

const HOST: &str = "https://hel.example.fi";
/// A public Endpoint that lists nothing, so it serves `websocket` (EP-104).
const PUBLIC: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaa";
/// An Endpoint for its own project.
const INTERNAL: &str = "cccccccccccccccccccccccccc";
/// A public Endpoint that turned the WebSocket off.
const DISABLED: &str = "dddddddddddddddddddddddddd";
/// A public Endpoint whose view the stub broker refuses to upgrade (its connection cap).
const BUSY: &str = "eeeeeeeeeeeeeeeeeeeeeeeeee";

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
    write("busy.yaml", &endpoint("helsinki-busy", BUSY, "public", ""));
    write("public-policy.yaml", &policy("public-bikes", "public"));
    write("staff.yaml", &policy("staff-bikes", "data-steward"));
    dir
}

type Upgrades = Arc<Mutex<Vec<HeaderMap>>>;

/// A broker whose view WebSocket echoes every text frame, answers `big:{n}` with n bytes and
/// `drop` by going away, and answers `503` for [`BUSY`].
async fn broker() -> (String, Upgrades) {
    let upgrades: Upgrades = Arc::default();
    let seen = Arc::clone(&upgrades);
    let app = Router::new().route(
        "/x/views/{view}/ws",
        get(
            move |Path(view): Path<String>, headers: HeaderMap, upgrade: WebSocketUpgrade| {
                let seen = Arc::clone(&seen);
                async move {
                    seen.lock().expect("the upgrade log").push(headers);
                    if view == BUSY {
                        return StatusCode::SERVICE_UNAVAILABLE.into_response();
                    }
                    upgrade.on_upgrade(|mut socket| async move {
                        while let Some(Ok(message)) = socket.recv().await {
                            if let AxumMessage::Text(text) = message {
                                // `drop`: the broker goes away without a close frame.
                                if text.as_str() == "drop" {
                                    return;
                                }
                                // `big:{n}`: one frame of n bytes, as a notification is.
                                let echo = match text.as_str().strip_prefix("big:") {
                                    Some(n) => "n".repeat(n.parse().expect("a size")),
                                    None => format!("echo:{}", text.as_str()),
                                };
                                if socket.send(AxumMessage::Text(echo.into())).await.is_err() {
                                    break;
                                }
                            }
                        }
                    })
                }
            },
        ),
    );
    (serve(app).await, upgrades)
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
    let (endpoints, ..) = store::load(&repo(test)).expect("the repository loads");
    let gateway = Gateway::new(
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
    let (upstream, upgrades) = broker().await;
    let at = gateway("anonymous", &upstream, &realm).await;
    assert_eq!(
        open(&at, INTERNAL, "", &[]).await.err(),
        Some(StatusCode::UNAUTHORIZED)
    );
    assert!(upgrades.lock().expect("log").is_empty());
}

/// A stranger's token gets on the WebSocket what it gets on REST.
#[tokio::test]
async fn a_strangers_token_is_refused_as_on_rest() {
    let realm = common::Realm::new();
    let (upstream, upgrades) = broker().await;
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
    assert!(upgrades.lock().expect("log").is_empty());
}

/// EP-107: a token in the URL is refused and never read, even a valid one.
#[tokio::test]
async fn a_token_in_the_query_is_refused() {
    let realm = common::Realm::new();
    let (upstream, upgrades) = broker().await;
    let at = gateway("query", &upstream, &realm).await;
    let token = person(&realm, "hel-mobility", INTERNAL);
    assert_eq!(
        open(&at, INTERNAL, &format!("?access_token={token}"), &[])
            .await
            .err(),
        Some(StatusCode::BAD_REQUEST)
    );
    assert!(upgrades.lock().expect("log").is_empty());
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

/// EP-106: a browser's token rides as a subprotocol entry; the answer names only `ngsi-ld-ws`,
/// the broker gets the pinned caller and never the token, and frames cross both ways.
#[tokio::test]
async fn frames_are_relayed_under_the_pinned_caller() {
    let realm = common::Realm::new();
    let (upstream, upgrades) = broker().await;
    let at = gateway("relay", &upstream, &realm).await;
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
    socket
        .send(Message::text(r#"{"metadata":{"x-op":"x-subscribe"}}"#))
        .await
        .expect("sent");
    let echoed = socket.next().await.expect("a frame").expect("readable");
    assert_eq!(
        echoed,
        Message::text(r#"echo:{"metadata":{"x-op":"x-subscribe"}}"#)
    );
    let upgrade = upgrades.lock().expect("log")[0].clone();
    assert_eq!(upgrade["ngsild-tenant"], "helsinki");
    assert_eq!(upgrade["x-jc-subject"], "someone-of-hel-mobility");
    assert!(!upgrade.contains_key("authorization"));
    assert!(!upgrade.contains_key("origin"));
    let protocols = upgrade
        .get("sec-websocket-protocol")
        .map(|value| value.to_str().unwrap_or_default().to_owned())
        .unwrap_or_default();
    assert!(
        !protocols.contains(&token),
        "the token never reaches the broker"
    );
}

/// The broker's own `Origin` rule refuses every browser, so the gateway judges it: a page on
/// another site is refused before the broker is asked.
#[tokio::test]
async fn a_foreign_origin_is_refused() {
    let realm = common::Realm::new();
    let (upstream, upgrades) = broker().await;
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
    assert!(upgrades.lock().expect("log").is_empty());
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
        assert!(
            !matches!(message, Message::Text(_)),
            "nothing came back from the broker"
        );
    }
    assert_eq!(code, Some(1009));
}

/// The broker's refusal of the view's upgrade is the gateway's HTTP answer, before any `101`.
#[tokio::test]
async fn a_broker_refusal_is_answered_before_the_upgrade() {
    let realm = common::Realm::new();
    let (upstream, _) = broker().await;
    let at = gateway("busy", &upstream, &realm).await;
    assert_eq!(
        open(&at, BUSY, "", &[]).await.err(),
        Some(StatusCode::SERVICE_UNAVAILABLE)
    );
}

/// T-4064: a plain GET (a browser tab, curl) is no upgrade. It is told how to use the address,
/// `426` with the subprotocol, the token rule, one `x-subscribe` message and the guide, never
/// axum's bare `400`; and the broker is not asked.
#[tokio::test]
async fn a_plain_get_is_told_how_to_use_the_websocket() {
    let realm = common::Realm::new();
    let (upstream, upgrades) = broker().await;
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

    assert!(
        upgrades.lock().expect("log").is_empty(),
        "no upgrade reached the broker"
    );
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

/// T-4131: the broker writes notifications of up to 1 MiB in one frame; one over the 16 KiB a
/// client may send reaches the client whole, and the connection stays up.
#[tokio::test]
async fn a_notification_over_the_client_bound_reaches_the_client() {
    let realm = common::Realm::new();
    let (upstream, _) = broker().await;
    let at = gateway("big-notification", &upstream, &realm).await;
    let (mut socket, _) = open(&at, PUBLIC, "", &[]).await.expect("upgraded");

    socket
        .send(Message::text("big:200000"))
        .await
        .expect("sent");
    let big = socket.next().await.expect("a frame").expect("readable");
    assert_eq!(big.len(), 200_000);

    socket.send(Message::text("after")).await.expect("sent");
    let echoed = socket.next().await.expect("a frame").expect("readable");
    assert_eq!(echoed, Message::text("echo:after"));
}

/// T-4131: when the broker's side ends without a close frame, the client gets 1011 rather than
/// a bare end of the stream.
#[tokio::test]
async fn a_broker_gone_mid_stream_closes_the_client_with_1011() {
    let realm = common::Realm::new();
    let (upstream, _) = broker().await;
    let at = gateway("broker-gone", &upstream, &realm).await;
    let (mut socket, _) = open(&at, PUBLIC, "", &[]).await.expect("upgraded");

    socket.send(Message::text("drop")).await.expect("sent");
    let mut code = None;
    while let Some(Ok(message)) = socket.next().await {
        if let Message::Close(frame) = message {
            code = frame.map(|frame| u16::from(frame.code));
            break;
        }
    }
    assert_eq!(code, Some(1011));
}
