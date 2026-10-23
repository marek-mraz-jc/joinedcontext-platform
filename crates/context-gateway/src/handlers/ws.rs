//! The WebSocket surface of an endpoint (EP-106, EP-107, EP-114, T-3885, T-4133): `{endpoint}/ws`
//! is served by the Endpoint's fan-out (ADR-N-049), one broker subscription for every client.
//!
//! Everything is decided before the client gets its `101`: the slug, the token (the
//! `Authorization` header, or the `bearer.<token>` subprotocol entry a browser can send), the
//! `Origin`, the caller's connection cap and the space's, so a refusal is an HTTP answer a
//! client can read. The connection's filters, their matching and the cut to the caller's grant
//! are the fan-out's (`crate::fanout`).

use crate::app::Gateway;
use crate::auth::subject::{admit, principal_of};
use crate::handlers::space_surface::escape;
use crate::middleware::rate_limit::address_key;
use crate::middleware::tenancy;
use crate::resolver::Endpoint;
use axum::body::Body;
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{FromRequestParts, Path, Request, State};
use axum::http::header::{
    ACCEPT, AUTHORIZATION, CONNECTION, CONTENT_TYPE, LINK, ORIGIN, SEC_WEBSOCKET_PROTOCOL, UPGRADE,
};
use axum::http::{HeaderMap, HeaderValue, Response, StatusCode};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use jc_core::kinds::{Audience, Representation};
use jc_core::ProblemDetails;
use serde_json::json;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

/// The subprotocol a client names, and the only one the gateway answers with (API/07 §1).
const PROTOCOL: &str = "ngsi-ld-ws";
/// The subprotocol entry that carries a browser's token (EP-106).
const BEARER: &str = "bearer.";
/// EP-107: the WebSocket connections one caller holds open at once.
pub const PER_CALLER: usize = 10;
/// The client's bounds (API/07 §3): a message over them closes the connection with 1009.
const MAX_MESSAGE: usize = 64 * 1024;
const MAX_FRAME: usize = 16 * 1024;
/// The longest a token caller's socket lives; it ends sooner when its access token expires,
/// since nothing re-checks the token on an open socket (API/07 §3, T-4874).
pub const TOKEN_LIFETIME: std::time::Duration = std::time::Duration::from_secs(300);
/// An anonymous socket's lifetime (API/07 §3).
const ANONYMOUS_LIFETIME: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

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
    let principal = principal_of(&subject);
    // An anonymous socket has no token to expire (T-4132), so only a day bounds it.
    let lifetime = if principal == "anonymous" {
        ANONYMOUS_LIFETIME
    } else {
        token_lifetime(&gateway, &endpoint, request.headers())
    };
    let caller = match principal {
        anonymous if anonymous == "anonymous" => address_key(request.headers()),
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
    let (mut parts, _) = request.into_parts();
    let upgrade = match WebSocketUpgrade::from_request_parts(&mut parts, &()).await {
        Ok(upgrade) => upgrade,
        Err(rejection) => return rejection.into_response(),
    };
    let principal = principal_of(&subject);
    let joined = gateway
        .hubs
        .join(&gateway, Arc::clone(&endpoint), subject, principal.clone());
    // Held until the upgrade runs: a failed upgrade drops it, and the connection leaves the hub.
    let joined = match joined {
        Ok(joined) => joined,
        Err(crate::fanout::socket::Refused::SpaceFull) => {
            metrics::counter!("jc_gateway_ws_refused_total", "reason" => "space-full").increment(1);
            return ProblemDetails::new(503, "space-full", "Service Unavailable")
                .with_detail(format!(
                    "a space holds at most {} WebSocket connections at once",
                    crate::fanout::socket::per_space()
                ))
                .into_response();
        }
        Err(crate::fanout::socket::Refused::HubsFull) => {
            tracing::warn!(space = %endpoint.space, "WebSocket refused: every fan-out hub has a client");
            metrics::counter!("jc_gateway_ws_refused_total", "reason" => "fan-out-full")
                .increment(1);
            return ProblemDetails::new(503, "fan-out-full", "Service Unavailable")
                .with_detail(
                    "the gateway serves as many Endpoint fan-outs as it may; try again later",
                )
                .into_response();
        }
        Err(crate::fanout::socket::Refused::NoIdentity) => {
            tracing::error!(space = %endpoint.space, "endpoint names an unusable space");
            return ProblemDetails::internal().into_response();
        }
    };
    tracing::info!(slug = %endpoint.slug, space = %endpoint.space, %principal, "WebSocket opened on the fan-out");
    upgrade
        .protocols([PROTOCOL])
        .max_message_size(MAX_MESSAGE)
        .max_frame_size(MAX_FRAME)
        .on_upgrade(move |client| async move {
            let (hub, conn, queue) = joined.take();
            let ended =
                crate::fanout::socket::session(client, hub, conn, queue, principal, lifetime).await;
            tracing::info!(slug = %endpoint.slug, closed = ?ended, "WebSocket closed");
            metrics::counter!("jc_gateway_ws_closed_total", "reason" => ended.reason())
                .increment(1);
            drop(held);
        })
}

/// How long a token caller's socket may live (T-4874): until its token's `exp`, never past
/// the gateway's cap, so reconnecting needs a fresh token. A token the realm does not verify
/// here (a data space caller's) keeps the cap.
fn token_lifetime(
    gateway: &Gateway,
    endpoint: &Endpoint,
    headers: &HeaderMap,
) -> std::time::Duration {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    let presented = headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok());
    let left = jc_core::auth::token::bearer(presented)
        .ok()
        .zip(gateway.verifier.as_deref())
        .and_then(|(raw, verifier)| verifier.verify(raw, &gateway.audiences_for(endpoint)).ok())
        .map(|claims| {
            std::time::Duration::from_secs(
                u64::try_from(claims.exp).unwrap_or(0).saturating_sub(now),
            )
        });
    left.map_or(gateway.ws_token_lifetime, |left| {
        left.min(gateway.ws_token_lifetime)
    })
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
    let example = subscribe_example(endpoint);
    let public = endpoint.audience == Audience::Public;
    let token = token_text(public);
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

pub(crate) use jc_dcat::ws_address;

/// One `x-subscribe` message for a type of this Endpoint (API/07 §2).
fn subscribe_example(endpoint: &Endpoint) -> serde_json::Value {
    json!({
        "metadata": { "x-op": "x-subscribe", "x-id": "s1" },
        "body": { "type": "Subscription", "entities": [{ "type": example_type(endpoint) }] },
    })
}

/// What a caller needs to present, said without naming any token.
fn token_text(public: bool) -> &'static str {
    if public {
        "none: this Endpoint is public"
    } else {
        "an access token for the Endpoint's audience, in the Authorization header or as the \
         bearer.<token> subprotocol entry"
    }
}

/// How an agent gets this Endpoint's changes as they happen (T-5045, API/07): the public
/// WebSocket address, the subprotocol, one `x-subscribe` message, the limits the upgrade above
/// enforces and the HTTP subscription alternative. `None` for an Endpoint that serves no
/// WebSocket, so nobody is pointed at an address that answers 404.
pub(crate) fn live_updates(
    endpoint: &Endpoint,
    base: &str,
    token_lifetime: std::time::Duration,
) -> Option<serde_json::Value> {
    if !endpoint.serves(Representation::WebSocket) {
        return None;
    }
    let public = endpoint.audience == Audience::Public;
    let mut lifetime = serde_json::Map::new();
    lifetime.insert("tokenSeconds".to_owned(), json!(token_lifetime.as_secs()));
    if public {
        lifetime.insert(
            "anonymousSeconds".to_owned(),
            json!(ANONYMOUS_LIFETIME.as_secs()),
        );
    }
    Some(json!({
        "address": ws_address(&format!("{base}{}/ws", endpoint.base_path)),
        "protocol": PROTOCOL,
        "token": token_text(public),
        "subscribe": subscribe_example(endpoint),
        "welcome": "the first message is x-welcome; keep its x-connection and x-resume-token \
                    and send x-resume after a reconnect",
        "limits": {
            "connectionsPerCaller": PER_CALLER,
            "connectionsPerSpace": crate::fanout::socket::per_space(),
            "maxMessageBytes": MAX_MESSAGE,
            "maxFrameBytes": MAX_FRAME,
            "lifetime": lifetime,
        },
        "httpSubscription": format!(
            "or have changes POSTed to your own HTTPS URL: create_subscription, or POST \
             {base}{}/ngsi-ld/v1/subscriptions, where describe_access lists createSubscription",
            endpoint.base_path
        ),
        "guide": format!("{base}/docs/API/07-websocket"),
    }))
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

/// This surface's routes; `app::router` merges them under its guards.
pub(crate) fn routes() -> Router<Arc<Gateway>> {
    Router::new()
        // Live updates: the broker view's WebSocket, relayed under the caller (EP-106, T-3885).
        .route("/api/endpoint/{slug}/ws", get(endpoint_ws))
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

    /// T-5045: an agent learns the public address, the subprotocol, one `x-subscribe` message
    /// and the limits the upgrade enforces; an Endpoint without the WebSocket names none.
    #[test]
    fn live_updates_name_the_socket_only_where_it_is_served() {
        let endpoint = crate::fanout::hub::tests_support::endpoint();
        let base = "https://data.example.fi";
        let live = live_updates(&endpoint, base, TOKEN_LIFETIME).unwrap_or_default();
        assert_eq!(live["address"], "wss://data.example.fi/api/endpoint/s/ws");
        assert_eq!(live["protocol"], PROTOCOL);
        assert_eq!(live["subscribe"]["metadata"]["x-op"], "x-subscribe");
        assert_eq!(live["subscribe"]["body"]["type"], "Subscription");
        assert_eq!(live["limits"]["connectionsPerCaller"], PER_CALLER);
        assert_eq!(live["limits"]["maxMessageBytes"], MAX_MESSAGE);
        assert_eq!(live["limits"]["lifetime"]["anonymousSeconds"], 86_400);
        assert_eq!(live["limits"]["lifetime"]["tokenSeconds"], 300);
        assert_eq!(live["token"], "none: this Endpoint is public");
        let http = live["httpSubscription"].as_str().unwrap_or_default();
        assert!(http.contains("https://data.example.fi/api/endpoint/s/ngsi-ld/v1/subscriptions"));
        // Only the public base: no token, no in-cluster host.
        let text = live.to_string();
        assert!(
            !text.contains("Bearer ") && !text.contains("svc.cluster"),
            "{text}"
        );

        let mut private = (*endpoint).clone();
        private.audience = jc_core::kinds::Audience::Organization;
        let live = live_updates(&private, base, TOKEN_LIFETIME).unwrap_or_default();
        assert!(live["limits"]["lifetime"]["anonymousSeconds"].is_null());
        assert!(live["token"]
            .as_str()
            .is_some_and(|t| t.contains("access token")));

        private.representations = vec![Representation::NgsiLd];
        assert_eq!(live_updates(&private, base, TOKEN_LIFETIME), None);
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
}
