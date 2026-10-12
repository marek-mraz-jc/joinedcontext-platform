//! The WebSocket surface of an endpoint (EP-106, EP-107, T-3885): `{endpoint}/ws` is relayed to
//! the broker view's `/x/views/{view}/ws` (Antares ADR-0026) under the verified caller.
//!
//! Everything is decided before the client gets its `101`: the slug, the token (the
//! `Authorization` header, or the `bearer.<token>` subprotocol entry a browser can send), the
//! `Origin`, the caller's connection cap and the broker's own upgrade of the view, so a refusal is
//! an HTTP answer a client can read. Then frames are relayed both ways, one at a time, so a slow
//! side holds the other back instead of filling a buffer. Subscriptions, their delivery and their
//! narrowing are the broker's (ADR-0026 rules 4 and 8).

use crate::app::{admit, principal_of, Gateway};
use crate::handlers::space_surface::escape;
use crate::middleware::rate_limit::caller_key;
use crate::middleware::tenancy;
use crate::resolver::Endpoint;
use axum::body::Body;
use axum::extract::ws::{self, WebSocket, WebSocketUpgrade};
use axum::extract::{FromRequestParts, Path, Request, State};
use axum::http::header::{
    ACCEPT, AUTHORIZATION, CONNECTION, CONTENT_TYPE, LINK, ORIGIN, SEC_WEBSOCKET_PROTOCOL, UPGRADE,
};
use axum::http::{HeaderMap, HeaderValue, Response, StatusCode};
use axum::response::IntoResponse;
use futures_util::{SinkExt, StreamExt};
use jc_core::kinds::{Audience, Representation};
use jc_core::ProblemDetails;
use serde_json::json;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, WebSocketConfig};
use tokio_tungstenite::tungstenite::{self as tungstenite, Message};

/// The subprotocol a client names, and the only one the gateway answers with (API/07 §1).
const PROTOCOL: &str = "ngsi-ld-ws";
/// The subprotocol entry that carries a browser's token (EP-106).
const BEARER: &str = "bearer.";
/// EP-107: the WebSocket connections one caller holds open at once.
pub const PER_CALLER: usize = 10;
/// The broker's bounds (ADR-0026 rule 10), held here too, so an oversized message never
/// crosses the hop.
const MAX_MESSAGE: usize = 64 * 1024;
const MAX_FRAME: usize = 16 * 1024;
/// RFC 6455 7.4.1: the message was too big to process.
const TOO_BIG: u16 = 1009;

/// The WebSocket connections each caller holds open (EP-107).
#[derive(Debug, Default)]
pub struct Open(Mutex<HashMap<String, usize>>);

impl Open {
    /// One more connection for `caller`; `false` at the cap.
    fn take(&self, caller: &str) -> bool {
        let mut open = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let held = open.entry(caller.to_owned()).or_default();
        if *held >= PER_CALLER {
            return false;
        }
        *held += 1;
        true
    }

    fn give_back(&self, caller: &str) {
        let mut open = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(held) = open.get_mut(caller) {
            *held = held.saturating_sub(1);
            if *held == 0 {
                open.remove(caller);
            }
        }
    }
}

/// A connection counted against its caller until it ends, however it ends.
struct Held {
    gateway: Arc<Gateway>,
    caller: String,
}

impl Drop for Held {
    fn drop(&mut self) {
        self.gateway.ws_open.give_back(&self.caller);
    }
}

/// The upgrade of `{endpoint}/ws` (EP-106, EP-107).
pub(crate) async fn endpoint_ws(
    State(gateway): State<Arc<Gateway>>,
    Path(slug): Path<String>,
    mut request: Request,
) -> Response<Body> {
    // Before routing, before authentication, before anything reads a header (EP-21).
    tenancy::strip_client_headers(&mut request);
    // EP-107: a token in a URL ends up in access logs and browser histories; it is refused,
    // never read, and the query is not logged.
    if request.uri().query().is_some_and(names_a_token) {
        return ProblemDetails::bad_request()
            .with_detail(
                "a token travels in the Authorization header or as the bearer.<token> \
                 subprotocol entry, never in the URL",
            )
            .into_response();
    }
    if let Err(refused) = bearer_from_protocol(request.headers_mut()) {
        return *refused;
    }
    let (endpoint, subject) = match admit(
        &gateway,
        &slug,
        Some(Representation::WebSocket),
        request.headers(),
    ) {
        Ok(admitted) => admitted,
        Err(problem) => return *problem,
    };
    // A browser tab or a plain curl is no upgrade: it is told how to use the address instead
    // of axum's bare 400 (T-4064). It holds no connection, so it counts against no cap.
    if !asks_to_upgrade(request.headers()) {
        return guide(&endpoint, gateway.base_url(), request.headers());
    }
    if !origin_allowed(request.headers(), gateway.base_url()) {
        tracing::info!(slug = %endpoint.slug, "WebSocket refused: a foreign Origin");
        return ProblemDetails::new(403, "origin-refused", "Forbidden")
            .with_detail("this Origin may not open the Endpoint's WebSocket")
            .into_response();
    }
    // A person or an account is one caller however many tokens it holds; an anonymous caller
    // is its address.
    let caller = match principal_of(&subject) {
        anonymous if anonymous == "anonymous" => caller_key(request.headers()),
        principal => principal,
    };
    if !gateway.ws_open.take(&caller) {
        let mut refused = ProblemDetails::new(429, "too-many-connections", "Too Many Requests")
            .with_detail(format!(
                "a caller holds at most {PER_CALLER} WebSocket connections at once"
            ))
            .into_response();
        refused
            .headers_mut()
            .insert("retry-after", HeaderValue::from_static("5"));
        return refused;
    }
    let held = Held {
        gateway: Arc::clone(&gateway),
        caller,
    };
    let view = endpoint.slug.clone();
    if let Err(problem) = tenancy::pin_caller(&mut request, &endpoint.space, &subject) {
        tracing::error!(space = %endpoint.space, %problem, "endpoint names an unusable space");
        return ProblemDetails::internal().into_response();
    }
    let (mut parts, _) = request.into_parts();
    let upgrade = match WebSocketUpgrade::from_request_parts(&mut parts, &()).await {
        Ok(upgrade) => upgrade,
        Err(rejection) => return rejection.into_response(),
    };
    let base = match gateway
        .broker
        .base_for(&format!("/x/views/{view}/ws"), &parts.headers)
    {
        Ok(base) => base,
        Err(error) => return ProblemDetails::from(error).into_response(),
    };
    let upstream = match upstream_request(&base, &view, &parts.headers) {
        Ok(upstream) => upstream,
        Err(refused) => return *refused,
    };
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_MESSAGE))
        .max_frame_size(Some(MAX_FRAME));
    let broker = match tokio_tungstenite::connect_async_with_config(upstream, Some(config), false)
        .await
    {
        Ok((broker, _)) => broker,
        Err(tungstenite::Error::Http(answer)) => return refused_by_broker(answer.status()),
        Err(error) => {
            tracing::warn!(slug = %endpoint.slug, %view, %error, "the broker's WebSocket did not open");
            return ProblemDetails::new(502, "broker-unavailable", "Bad Gateway")
                .with_detail("the broker did not open the view's WebSocket")
                .into_response();
        }
    };
    tracing::info!(
        slug = %endpoint.slug,
        space = %endpoint.space,
        %view,
        principal = %principal_of(&subject),
        "WebSocket relayed to the broker's view"
    );
    upgrade
        .protocols([PROTOCOL])
        .max_message_size(MAX_MESSAGE)
        .max_frame_size(MAX_FRAME)
        .on_upgrade(move |client| relay(client, broker, held, endpoint.slug.clone()))
}

/// Whether the request asks for a WebSocket at all (RFC 6455 §4.1: `Upgrade: websocket`).
fn asks_to_upgrade(headers: &HeaderMap) -> bool {
    headers
        .get(UPGRADE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(',')
                .any(|token| token.trim().eq_ignore_ascii_case("websocket"))
        })
}

/// The answer to a plain `GET` of `{endpoint}/ws` (T-4064, API/07 §1): `426` with the address,
/// the subprotocol, whether a token is needed, one `x-subscribe` message and the guide, as an
/// HTML page for a browser and a problem+json otherwise.
fn guide(endpoint: &Endpoint, base: &str, headers: &HeaderMap) -> Response<Body> {
    let address = ws_address(&format!("{base}{}/ws", endpoint.base_path));
    let help = format!("{base}/docs/API/07-websocket");
    let example = json!({
        "metadata": { "x-op": "x-subscribe", "x-id": "s1" },
        "body": { "type": "Subscription", "entities": [{ "type": example_type(endpoint) }] },
    });
    let public = endpoint.audience == Audience::Public;
    let token = if public {
        "none: this Endpoint is public"
    } else {
        "an access token for the Endpoint's audience, in the Authorization header or as the \
         bearer.<token> subprotocol entry"
    };
    let credential = if public {
        ""
    } else {
        " -H \"Authorization: Bearer $TOKEN\""
    };
    let websocat = format!("websocat --protocol {PROTOCOL}{credential} \"{address}\"");
    let html = headers
        .get(ACCEPT)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.contains("text/html"));
    let mut response = if html {
        let page = format!(
            "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
             <title>Live updates over WebSocket</title></head><body>\
             <h1>Live updates over WebSocket</h1><p>This address is a WebSocket. Open \
             <code>{address}</code> with the subprotocol <code>{PROTOCOL}</code>, then send an \
             <code>x-subscribe</code> message; each change the Endpoint lets you read arrives as \
             a message.</p><p>Token: {token}.</p><p>Example message:</p><pre>{example}</pre>\
             <p>With websocat:</p><pre>{websocat}</pre><p><a href=\"{help}\">How to use it</a></p>\
             </body></html>",
            address = escape(&address),
            token = escape(token),
            example = escape(&example.to_string()),
            websocat = escape(&websocat),
            help = escape(&help),
        );
        (
            StatusCode::UPGRADE_REQUIRED,
            [(CONTENT_TYPE, "text/html; charset=utf-8")],
            page,
        )
            .into_response()
    } else {
        ProblemDetails::new(426, "upgrade-required", "Upgrade Required")
            .with_detail(format!(
                "open {address} as a WebSocket with the subprotocol {PROTOCOL}, then send an \
                 x-subscribe message"
            ))
            .with_extension("address", json!(address))
            .with_extension("protocol", json!(PROTOCOL))
            .with_extension("token", json!(token))
            .with_extension("example", example)
            .with_extension("websocat", json!(websocat))
            .with_extension("guide", json!(help))
            .into_response()
    };
    let answer = response.headers_mut();
    answer.insert(UPGRADE, HeaderValue::from_static("websocket"));
    answer.insert(CONNECTION, HeaderValue::from_static("Upgrade"));
    if let Ok(link) = HeaderValue::from_str(&format!("<{help}>; rel=\"help\"")) {
        answer.insert(LINK, link);
    }
    response
}

/// An `http(s)` address as the `ws(s)` one a WebSocket client opens.
pub(crate) fn ws_address(url: &str) -> String {
    match url.split_once("://") {
        Some(("https", rest)) => format!("wss://{rest}"),
        Some(("http", rest)) => format!("ws://{rest}"),
        _ => url.to_owned(),
    }
}

/// The first entity type the Endpoint's policies name, else the first class of its models: a
/// type the example can subscribe to.
fn example_type(endpoint: &Endpoint) -> String {
    endpoint
        .policies
        .iter()
        .flat_map(|policy| &policy.information)
        .flat_map(|information| &information.entities)
        .map(|selector| selector.entity_type.as_str())
        .chain(
            endpoint
                .models
                .iter()
                .flat_map(|model| model.classes.iter().map(String::as_str)),
        )
        .find(|name| !name.is_empty() && *name != "*")
        .unwrap_or("Type")
        .to_owned()
}

/// Whether a query names a credential (`access_token`, `token`, `bearer`, any case).
fn names_a_token(query: &str) -> bool {
    query.split('&').any(|pair| {
        let name = pair
            .split('=')
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        matches!(
            name.as_str(),
            "access_token" | "token" | "bearer" | "authorization"
        )
    })
}

/// A browser cannot set `Authorization` on a WebSocket, so its token arrives as the subprotocol
/// entry `bearer.<token>` (EP-106). It moves into `Authorization` for [`admit`], and the
/// subprotocol list keeps only the other entries, so the token is never echoed or relayed.
fn bearer_from_protocol(headers: &mut HeaderMap) -> Result<(), Box<Response<Body>>> {
    let offered: Vec<String> = headers
        .get_all(SEC_WEBSOCKET_PROTOCOL)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(|entry| entry.trim().to_owned())
        .filter(|entry| !entry.is_empty())
        .collect();
    let (tokens, others): (Vec<String>, Vec<String>) = offered
        .into_iter()
        .partition(|entry| entry.starts_with(BEARER));
    let Some(token) = tokens.first() else {
        return Ok(());
    };
    let both = headers.contains_key(AUTHORIZATION);
    let header = HeaderValue::from_str(&format!("Bearer {}", &token[BEARER.len()..]));
    let (false, 1, Ok(header)) = (both, tokens.len(), header) else {
        return Err(Box::new(
            ProblemDetails::bad_request()
                .with_detail("name one token, in the Authorization header or in one bearer. entry")
                .into_response(),
        ));
    };
    headers.insert(AUTHORIZATION, header);
    headers.remove(SEC_WEBSOCKET_PROTOCOL);
    if let Ok(rest) = HeaderValue::from_str(&others.join(", ")) {
        if !others.is_empty() {
            headers.insert(SEC_WEBSOCKET_PROTOCOL, rest);
        }
    }
    Ok(())
}

/// A client with no `Origin` is no browser page (websocat, a service); a page may connect from
/// the gateway's own public origin. The broker refuses every browser `Origin` by default
/// (ADR-0026 rule 10), so the gateway judges it and never relays it.
fn origin_allowed(headers: &HeaderMap, public_url: &str) -> bool {
    let Some(origin) = headers.get(ORIGIN) else {
        return true;
    };
    let own = public_url
        .split_once("://")
        .map(|(scheme, rest)| format!("{scheme}://{}", rest.split('/').next().unwrap_or(rest)));
    own.is_some_and(|own| origin.as_bytes().eq_ignore_ascii_case(own.as_bytes()))
}

/// The broker's upgrade of the view, carrying the pinned tenant and caller and nothing the
/// client sent of its own.
fn upstream_request(
    base: &str,
    view: &str,
    pinned: &HeaderMap,
) -> Result<tungstenite::handshake::client::Request, Box<Response<Body>>> {
    // ponytail: the broker is a plain in-cluster address (Linkerd's mTLS under it); an
    // https broker base needs tungstenite's TLS feature and is refused until a deployment
    // has one.
    let Some(authority) = base.strip_prefix("http://") else {
        tracing::error!(%base, "the WebSocket relay reaches only an http:// broker");
        return Err(Box::new(
            ProblemDetails::new(502, "broker-unavailable", "Bad Gateway").into_response(),
        ));
    };
    let authority = authority.trim_end_matches('/');
    let mut upstream = format!("ws://{authority}/x/views/{view}/ws")
        .into_client_request()
        .map_err(|_| Box::new(ProblemDetails::internal().into_response()))?;
    for (name, value) in pinned {
        if name == tenancy::TENANT || name.as_str().starts_with("x-jc-") {
            upstream.headers_mut().append(name.clone(), value.clone());
        }
    }
    Ok(upstream)
}

/// The broker refused the upgrade: its status, in the gateway's own words.
fn refused_by_broker(status: StatusCode) -> Response<Body> {
    match status {
        StatusCode::FORBIDDEN => ProblemDetails::new(403, "forbidden", "Forbidden")
            .with_detail("the Endpoint's Policy grants this caller no notification")
            .into_response(),
        StatusCode::NOT_FOUND => ProblemDetails::not_found().into_response(),
        StatusCode::SERVICE_UNAVAILABLE => {
            ProblemDetails::new(503, "too-many-connections", "Service Unavailable")
                .with_detail("the broker holds as many WebSocket connections as it takes")
                .into_response()
        }
        other => {
            tracing::warn!(status = %other, "the broker refused the WebSocket upgrade");
            ProblemDetails::new(502, "broker-unavailable", "Bad Gateway").into_response()
        }
    }
}

type Upstream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// How the client side ended.
#[derive(Debug, PartialEq, Eq)]
enum Ended {
    Closed,
    TooBig,
}

/// Frames both ways until either side ends; the connection is counted until then.
async fn relay(client: WebSocket, broker: Upstream, held: Held, slug: String) {
    let (mut to_client, mut from_client) = client.split();
    let (mut to_broker, mut from_broker) = broker.split();
    let ended = {
        let up = async {
            while let Some(read) = from_client.next().await {
                let message = match read {
                    Ok(message) => message,
                    Err(error) if too_big(&error) => return Ended::TooBig,
                    Err(_) => return Ended::Closed,
                };
                let closing = matches!(message, ws::Message::Close(_));
                let Some(message) = upstream_message(message) else {
                    continue;
                };
                if to_broker.send(message).await.is_err() || closing {
                    break;
                }
            }
            Ended::Closed
        };
        let down = async {
            while let Some(Ok(message)) = from_broker.next().await {
                let closing = matches!(message, Message::Close(_));
                let Some(message) = client_message(message) else {
                    continue;
                };
                if to_client.send(message).await.is_err() || closing {
                    break;
                }
            }
        };
        tokio::select! {
            ended = up => ended,
            () = down => Ended::Closed,
        }
    };
    if ended == Ended::TooBig {
        let frame = ws::CloseFrame {
            code: TOO_BIG,
            reason: "a message over 64 KiB".into(),
        };
        let _ = to_client.send(ws::Message::Close(Some(frame))).await;
    }
    let _ = to_broker.close().await;
    tracing::info!(%slug, closed = ?ended, "WebSocket closed");
    drop(held);
}

/// Whether a read failed because the client broke the message or frame bound.
fn too_big(error: &axum::Error) -> bool {
    std::error::Error::source(error)
        .and_then(|source| source.downcast_ref::<tungstenite::Error>())
        .is_some_and(|error| matches!(error, tungstenite::Error::Capacity(_)))
}

/// A client frame as the broker receives it. Ping and pong are each side's own (ADR-0026
/// rule 5), so they are not relayed.
fn upstream_message(message: ws::Message) -> Option<Message> {
    match message {
        ws::Message::Text(text) => Some(Message::text(text.as_str())),
        ws::Message::Binary(bytes) => Some(Message::Binary(bytes)),
        ws::Message::Close(frame) => Some(Message::Close(frame.map(|frame| CloseFrame {
            code: CloseCode::from(frame.code),
            reason: frame.reason.as_str().into(),
        }))),
        ws::Message::Ping(_) | ws::Message::Pong(_) => None,
    }
}

/// A broker frame as the client receives it.
fn client_message(message: Message) -> Option<ws::Message> {
    match message {
        Message::Text(text) => Some(ws::Message::Text(text.as_str().into())),
        Message::Binary(bytes) => Some(ws::Message::Binary(bytes)),
        Message::Close(frame) => Some(ws::Message::Close(frame.map(|frame| ws::CloseFrame {
            code: u16::from(frame.code),
            reason: frame.reason.as_str().into(),
        }))),
        Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_query_naming_a_token_is_seen() {
        assert!(names_a_token("access_token=x"));
        assert!(names_a_token("a=1&Token=x"));
        assert!(names_a_token("bearer"));
        assert!(!names_a_token("type=Car&tokens=3"));
        assert!(!names_a_token(""));
    }

    #[test]
    fn the_subprotocol_token_moves_to_authorization() {
        let mut headers = HeaderMap::new();
        headers.insert(
            SEC_WEBSOCKET_PROTOCOL,
            HeaderValue::from_static("ngsi-ld-ws, bearer.abc.def"),
        );
        assert!(bearer_from_protocol(&mut headers).is_ok());
        assert_eq!(headers[AUTHORIZATION], "Bearer abc.def");
        assert_eq!(headers[SEC_WEBSOCKET_PROTOCOL], "ngsi-ld-ws");

        let mut two = HeaderMap::new();
        two.insert(
            SEC_WEBSOCKET_PROTOCOL,
            HeaderValue::from_static("bearer.a, bearer.b"),
        );
        assert!(bearer_from_protocol(&mut two).is_err(), "two tokens");

        let mut both = HeaderMap::new();
        both.insert(AUTHORIZATION, HeaderValue::from_static("Bearer a"));
        both.insert(SEC_WEBSOCKET_PROTOCOL, HeaderValue::from_static("bearer.b"));
        assert!(
            bearer_from_protocol(&mut both).is_err(),
            "a header and an entry"
        );

        let mut none = HeaderMap::new();
        none.insert(
            SEC_WEBSOCKET_PROTOCOL,
            HeaderValue::from_static("ngsi-ld-ws"),
        );
        assert!(bearer_from_protocol(&mut none).is_ok());
        assert!(!none.contains_key(AUTHORIZATION));
    }

    #[test]
    fn only_the_gateways_own_origin_or_none_may_connect() {
        let with = |origin: &'static str| {
            let mut headers = HeaderMap::new();
            headers.insert(ORIGIN, HeaderValue::from_static(origin));
            headers
        };
        let public = "https://data.example.fi/base";
        assert!(origin_allowed(&HeaderMap::new(), public));
        assert!(origin_allowed(&with("https://data.example.fi"), public));
        assert!(origin_allowed(&with("HTTPS://DATA.EXAMPLE.FI"), public));
        assert!(!origin_allowed(&with("https://evil.example"), public));
        assert!(!origin_allowed(&with("https://data.example.fi"), ""));
    }

    #[test]
    fn a_caller_holds_at_most_its_cap() {
        let open = Open::default();
        for _ in 0..PER_CALLER {
            assert!(open.take("a"));
        }
        assert!(!open.take("a"));
        assert!(open.take("b"), "another caller has its own");
        open.give_back("a");
        assert!(open.take("a"));
        open.give_back("nobody");
    }

    #[test]
    fn upstream_carries_only_the_pinned_caller() {
        let mut pinned = HeaderMap::new();
        pinned.insert(tenancy::TENANT, HeaderValue::from_static("ovzdusie"));
        pinned.insert("x-jc-subject", HeaderValue::from_static("someone"));
        pinned.insert(ORIGIN, HeaderValue::from_static("https://evil.example"));
        pinned.insert(AUTHORIZATION, HeaderValue::from_static("Bearer t"));
        let upstream = upstream_request("http://antares:1026/", "air", &pinned).expect("built");
        assert_eq!(upstream.uri(), "ws://antares:1026/x/views/air/ws");
        assert_eq!(upstream.headers()["ngsild-tenant"], "ovzdusie");
        assert_eq!(upstream.headers()["x-jc-subject"], "someone");
        assert!(!upstream.headers().contains_key(ORIGIN));
        assert!(!upstream.headers().contains_key(AUTHORIZATION));
        assert!(upstream_request("https://antares", "air", &pinned).is_err());
    }

    #[test]
    fn frames_cross_unchanged_and_pings_stay() {
        assert_eq!(
            upstream_message(ws::Message::Text("{}".into())),
            Some(Message::text("{}"))
        );
        assert_eq!(
            upstream_message(ws::Message::Ping(Default::default())),
            None
        );
        assert_eq!(
            client_message(Message::Close(Some(CloseFrame {
                code: CloseCode::from(1013),
                reason: "overflow".into()
            }))),
            Some(ws::Message::Close(Some(ws::CloseFrame {
                code: 1013,
                reason: "overflow".into()
            })))
        );
        assert_eq!(client_message(Message::Pong(Default::default())), None);
    }
}
