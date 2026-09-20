//! Edge cases of `proxy::Broker::send` (T-1942; EP-01, GW20, GW21, R46, T-0005).
//!
//! Contract, one sentence: `send` forwards one request to the broker the gateway was configured
//! with and to no other host, relays the path exactly as it came off the wire, carries no
//! hop-by-hop header across in either direction, and tells the caller nothing about the topology
//! behind the endpoint when the hop fails.
//!
//! The unit tests in `proxy.rs` cover the header copying by itself. These run the hop: a real
//! upstream on loopback that records what arrived, and the shapes a caller can put in the path.

use axum::body::Body;
use axum::http::header::{HeaderName, HeaderValue, AUTHORIZATION, CONNECTION, HOST};
use axum::http::{HeaderMap, Method, StatusCode};
use context_gateway::proxy::{Broker, ProxyError};
use jc_core::ProblemDetails;
use std::sync::{Arc, Mutex};

/// What the upstream was asked.
#[derive(Clone, Default)]
struct Asked {
    method: String,
    uri: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

type Log = Arc<Mutex<Vec<Asked>>>;

/// An upstream that records every request and answers `status` with `body` and `headers`.
async fn upstream(
    status: u16,
    body: &'static str,
    headers: Vec<(&'static str, &'static str)>,
) -> (String, Log) {
    let log: Log = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&log);
    let app = axum::Router::new().fallback(axum::routing::any(
        move |request: axum::extract::Request| {
            let log = Arc::clone(&recorded);
            let headers = headers.clone();
            async move {
                let method = request.method().to_string();
                let uri = request.uri().to_string();
                let seen = request
                    .headers()
                    .iter()
                    .map(|(name, value)| {
                        (
                            name.to_string(),
                            String::from_utf8_lossy(value.as_bytes()).into_owned(),
                        )
                    })
                    .collect();
                let bytes = axum::body::to_bytes(request.into_body(), 1024 * 1024)
                    .await
                    .expect("a body");
                log.lock().expect("the log").push(Asked {
                    method,
                    uri,
                    headers: seen,
                    body: bytes.to_vec(),
                });
                let mut answer = axum::http::Response::builder()
                    .status(StatusCode::from_u16(status).expect("a status"));
                for (name, value) in headers {
                    answer = answer.header(name, value);
                }
                answer.body(Body::from(body)).expect("an answer")
            }
        },
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a free port");
    let address = listener.local_addr().expect("the bound address");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{address}"), log)
}

fn headers(of: &[(&str, &str)]) -> HeaderMap {
    let mut map = HeaderMap::new();
    for (name, value) in of {
        map.append(
            HeaderName::try_from(*name).expect("a header name"),
            HeaderValue::from_str(value).expect("a header value"),
        );
    }
    map
}

impl Asked {
    fn header(&self, name: &str) -> Vec<&str> {
        self.headers
            .iter()
            .filter(|(seen, _)| seen == name)
            .map(|(_, value)| value.as_str())
            .collect()
    }
}

async fn read(response: axum::http::Response<Body>) -> (StatusCode, HeaderMap, Vec<u8>) {
    let (parts, body) = response.into_parts();
    let bytes = axum::body::to_bytes(body, 1024 * 1024)
        .await
        .expect("a body");
    (parts.status, parts.headers, bytes.to_vec())
}

#[tokio::test]
async fn the_path_and_query_reach_the_broker_exactly_as_they_came_off_the_wire() {
    // An entity path holds a URN, whose colons and percent-escapes are part of the id. Decoding
    // or re-encoding it here addresses a different entity, or none.
    let (base, log) = upstream(200, "[]", Vec::new()).await;
    let broker = Broker::new(base);
    for path in [
        "/ngsi-ld/v1/entities/urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:ovzdusie:s-1",
        "/ngsi-ld/v1/entities/urn%3Angsi-ld%3AAirQualityObserved%3Abb.sk%3Aair%3As-1/attrs",
        "/ngsi-ld/v1/entities?q=name%3D%3D%22Radvan%2FSt%C5%99ed%22&limit=1",
        "/ngsi-ld/v1/entities?q=a%2520b",
        "/ngsi-ld/v1/entities/",
    ] {
        broker
            .send(Method::GET, path, HeaderMap::new(), Body::empty())
            .await
            .expect("the broker answered");
        let asked = log
            .lock()
            .expect("the log")
            .last()
            .cloned()
            .expect("a request");
        assert_eq!(asked.uri, path, "the path was rewritten on the way");
    }
}

#[tokio::test]
async fn a_caller_cannot_move_the_hop_to_another_host_through_the_path() {
    // GW20: the upstream is fixed at start-up and the path comes from the route the router
    // matched, so it begins with `/`. A path that looks like a URL must not become one, and the
    // gateway has to fail rather than resolve something halfway.
    //
    // A `path_and_query` that does *not* begin with `/` is glued straight onto the authority, and
    // `@evil.example/x` then names another host. No caller passes one today — `app.rs` builds
    // `/ngsi-ld/v1{path}?…` and `notifications::split` returns a path starting with `/` — so the
    // case is filed as T-2337 with its own red test rather than asserted here.
    let (base, log) = upstream(200, "[]", Vec::new()).await;
    let broker = Broker::new(base);

    for hostile in [
        "/../../admin",
        "http://evil.example/ngsi-ld/v1/entities",
        "https://evil.example/ngsi-ld/v1/entities",
    ] {
        match broker
            .send(Method::GET, hostile, HeaderMap::new(), Body::empty())
            .await
        {
            Err(error) => assert!(matches!(error, ProxyError::Uri(_)), "{hostile}: {error}"),
            Ok(_) => {
                let asked = log
                    .lock()
                    .expect("the log")
                    .last()
                    .cloned()
                    .expect("a request");
                assert_eq!(asked.uri, hostile, "{hostile} was rewritten");
            }
        }
        let host: Vec<String> = log
            .lock()
            .expect("the log")
            .iter()
            .flat_map(|asked| {
                asked
                    .header("host")
                    .iter()
                    .map(|h| (*h).to_owned())
                    .collect::<Vec<_>>()
            })
            .collect();
        assert!(
            host.iter().all(|seen| !seen.contains("evil")),
            "{hostile} reached {host:?}"
        );
    }

    // A protocol-relative path stays a path: the authority is the one the gateway holds.
    broker
        .send(
            Method::GET,
            "//evil.example/ngsi-ld/v1/entities",
            HeaderMap::new(),
            Body::empty(),
        )
        .await
        .expect("the configured broker answered");
    let asked = log
        .lock()
        .expect("the log")
        .last()
        .cloned()
        .expect("a request");
    assert_eq!(asked.uri, "//evil.example/ngsi-ld/v1/entities");
    assert!(
        asked
            .header("host")
            .iter()
            .all(|host| !host.contains("evil")),
        "{:?}",
        asked.header("host")
    );
}

#[tokio::test]
async fn the_authority_the_broker_sees_is_the_gateways_and_never_the_clients() {
    // A forged `Host` is how a caller reaches a virtual host it was not given. The header the
    // client sent is dropped and hyper writes the one belonging to the configured upstream.
    let (base, log) = upstream(200, "[]", Vec::new()).await;
    let address = base.trim_start_matches("http://").to_owned();
    Broker::new(base)
        .send(
            Method::GET,
            "/ngsi-ld/v1/entities",
            headers(&[("host", "broker.internal:9999")]),
            Body::empty(),
        )
        .await
        .expect("answered");
    let asked = log
        .lock()
        .expect("the log")
        .last()
        .cloned()
        .expect("a request");
    assert_eq!(asked.header("host"), vec![address.as_str()]);
}

#[tokio::test]
async fn a_hop_by_hop_header_does_not_cross_in_either_direction() {
    // RFC 9110 §7.6.1: they describe one TCP connection and this is two. `Upgrade` crossing is a
    // protocol switch nobody asked for; `Proxy-Authorization` crossing hands a credential on.
    let (base, log) = upstream(
        200,
        "[]",
        vec![
            ("connection", "keep-alive"),
            ("keep-alive", "timeout=5"),
            ("proxy-authenticate", "Basic realm=\"broker\""),
            ("content-type", "application/ld+json"),
        ],
    )
    .await;
    let answer = Broker::new(base)
        .send(
            Method::GET,
            "/ngsi-ld/v1/entities",
            headers(&[
                ("te", "trailers"),
                ("trailer", "x-checksum"),
                ("upgrade", "websocket"),
                ("proxy-authorization", "Basic c2VjcmV0"),
                ("accept", "application/ld+json"),
            ]),
            Body::empty(),
        )
        .await
        .expect("answered");

    let asked = log
        .lock()
        .expect("the log")
        .last()
        .cloned()
        .expect("a request");
    for gone in [
        "te",
        "trailer",
        "upgrade",
        "proxy-authorization",
        "keep-alive",
    ] {
        assert!(
            asked.header(gone).is_empty(),
            "{gone} crossed to the broker"
        );
    }
    assert_eq!(asked.header("accept"), vec!["application/ld+json"]);

    let (_, back, _) = read(answer).await;
    for gone in ["keep-alive", "proxy-authenticate"] {
        assert!(
            back.get(gone).is_none(),
            "{gone} crossed back to the caller"
        );
    }
    assert_eq!(
        back.get("content-type").map(HeaderValue::as_bytes),
        Some(&b"application/ld+json"[..])
    );
}

#[tokio::test]
async fn a_connection_header_takes_the_headers_it_names_with_it() {
    // `Connection: x-tenant` is the documented way to mark a header as belonging to this hop, and
    // it is also how a caller tries to smuggle one past a filter that only knows the fixed list.
    let (base, log) = upstream(
        200,
        "[]",
        vec![
            ("connection", "x-broker-note"),
            ("x-broker-note", "internal"),
        ],
    )
    .await;
    let answer = Broker::new(base)
        .send(
            Method::GET,
            "/ngsi-ld/v1/entities",
            headers(&[
                ("connection", "x-forwarded-tenant, X-Trace"),
                ("x-forwarded-tenant", "another-space"),
                ("x-trace", "1"),
                ("x-kept", "yes"),
            ]),
            Body::empty(),
        )
        .await
        .expect("answered");

    let asked = log
        .lock()
        .expect("the log")
        .last()
        .cloned()
        .expect("a request");
    for gone in ["connection", "x-forwarded-tenant", "x-trace"] {
        assert!(asked.header(gone).is_empty(), "{gone} crossed");
    }
    assert_eq!(
        asked.header("x-kept"),
        vec!["yes"],
        "only the named ones go"
    );

    let (_, back, _) = read(answer).await;
    assert!(
        back.get("x-broker-note").is_none(),
        "the answer's named header crossed"
    );
}

#[tokio::test]
async fn every_value_of_a_repeated_header_is_relayed_and_not_the_first_one_only() {
    // `Link` and `Accept` arrive repeated, and an NGSI-LD context lives in `Link`. Keeping one
    // value would change the meaning of the request.
    let (base, log) = upstream(
        200,
        "[]",
        vec![("link", "<a>; rel=x"), ("link", "<b>; rel=y")],
    )
    .await;
    let answer = Broker::new(base)
        .send(
            Method::GET,
            "/ngsi-ld/v1/entities",
            headers(&[
                ("link", "<one>; rel=context"),
                ("link", "<two>; rel=context"),
            ]),
            Body::empty(),
        )
        .await
        .expect("answered");

    let asked = log
        .lock()
        .expect("the log")
        .last()
        .cloned()
        .expect("a request");
    assert_eq!(
        asked.header("link"),
        vec!["<one>; rel=context", "<two>; rel=context"]
    );
    let (_, back, _) = read(answer).await;
    assert_eq!(back.get_all("link").iter().count(), 2);
}

#[tokio::test]
async fn the_status_and_the_body_the_broker_gave_are_the_ones_the_caller_gets() {
    // The gateway answers for the broker, so a 409 stays a 409: an error rewritten here is an
    // error the client cannot act on. The body is bytes, not JSON to be re-serialized.
    for (status, body) in [
        (201u16, ""),
        (204, ""),
        (400, "{\"type\":\"urn:x\"}"),
        (404, "not found, in plain words"),
        (409, "\u{feff}already exists"),
        (429, ""),
        (500, "<html>broker</html>"),
    ] {
        let leaked: &'static str = Box::leak(body.to_owned().into_boxed_str());
        let (base, _) = upstream(status, leaked, Vec::new()).await;
        let answer = Broker::new(base)
            .send(
                Method::POST,
                "/ngsi-ld/v1/entities",
                HeaderMap::new(),
                Body::from(body),
            )
            .await
            .expect("answered");
        let (seen, _, bytes) = read(answer).await;
        assert_eq!(seen.as_u16(), status);
        if status != 204 {
            assert_eq!(
                bytes,
                body.as_bytes(),
                "the body was changed on the way back"
            );
        }
    }
}

#[tokio::test]
async fn the_method_and_the_body_reach_the_broker_as_they_were_sent() {
    let (base, log) = upstream(200, "", Vec::new()).await;
    let broker = Broker::new(base);
    let payload = "{\"id\":\"urn:ngsi-ld:X:o:s:1\",\"pm10\":{\"value\":3.5}}";
    for method in [Method::POST, Method::PATCH, Method::PUT, Method::DELETE] {
        broker
            .send(
                method.clone(),
                "/ngsi-ld/v1/entities",
                HeaderMap::new(),
                Body::from(payload),
            )
            .await
            .expect("answered");
        let asked = log
            .lock()
            .expect("the log")
            .last()
            .cloned()
            .expect("a request");
        assert_eq!(asked.method, method.as_str());
        assert_eq!(asked.body, payload.as_bytes());
    }
}

#[tokio::test]
async fn a_body_larger_than_one_buffer_arrives_whole() {
    // The body is streamed rather than collected, and a chunked payload is what a bulk upsert is.
    let (base, log) = upstream(200, "", Vec::new()).await;
    let payload = "x".repeat(300_000);
    Broker::new(base)
        .send(
            Method::POST,
            "/ngsi-ld/v1/entityOperations/upsert",
            HeaderMap::new(),
            Body::from(payload.clone()),
        )
        .await
        .expect("answered");
    let asked = log
        .lock()
        .expect("the log")
        .last()
        .cloned()
        .expect("a request");
    assert_eq!(asked.body.len(), payload.len());
}

#[tokio::test]
async fn an_empty_path_addresses_the_root_of_the_broker_and_not_a_broken_uri() {
    let (base, log) = upstream(200, "", Vec::new()).await;
    Broker::new(base)
        .send(Method::GET, "", HeaderMap::new(), Body::empty())
        .await
        .expect("answered");
    let asked = log
        .lock()
        .expect("the log")
        .last()
        .cloned()
        .expect("a request");
    assert_eq!(asked.uri, "/");
}

#[tokio::test]
async fn a_broker_that_does_not_answer_is_a_502_that_names_neither_host_nor_port() {
    // GW6, R20: the caller learns that the platform is at fault. The address of the broker, which
    // no client is meant to know, stays out of the body.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a free port");
    let closed = listener.local_addr().expect("the bound address");
    drop(listener);

    let error = Broker::new(format!("http://{closed}"))
        .send(
            Method::GET,
            "/ngsi-ld/v1/entities",
            HeaderMap::new(),
            Body::empty(),
        )
        .await
        .expect_err("nothing answered");
    assert!(matches!(error, ProxyError::Unreachable(_)), "{error}");

    let problem = ProblemDetails::from(error);
    assert_eq!(problem.status, 502);
    let said = serde_json::to_string(&problem).expect("the body");
    assert!(!said.contains("127.0.0.1"), "{said}");
    assert!(!said.contains(&closed.port().to_string()), "{said}");
}

#[tokio::test]
async fn a_path_that_is_not_absolute_never_becomes_another_authority() {
    // T-2337: `self.base` ends at the authority, so a path without a leading `/` is parsed as
    // more authority — `@evil.example/x` makes the broker's address the userinfo of another host
    // and the hop leaves the upstream GW20 fixed at start-up. No caller writes one today; the
    // refusal is what keeps that true when one is rewritten.
    let (base, log) = upstream(200, "[]", Vec::new()).await;
    let broker = Broker::new(base);

    // Not `""`: an empty path is the broker's own root (`an_empty_path_addresses_the_root_…`),
    // it carries no authority to be mistaken for one.
    for hostile in [
        "@evil.example/x",
        "%2f@evil/x",
        "ngsi-ld/v1/entities",
        "evil.example",
    ] {
        let refused = broker
            .send(Method::GET, hostile, HeaderMap::new(), Body::empty())
            .await
            .expect_err("a path that is not absolute is refused");
        assert!(
            matches!(refused, ProxyError::Uri(_)),
            "{hostile}: {refused}"
        );
    }
    assert!(
        log.lock().expect("the log").is_empty(),
        "nothing was forwarded"
    );
}

#[tokio::test]
async fn the_refusal_of_a_relative_path_is_a_502_that_names_no_host() {
    let error = Broker::new("http://broker.jc-context-broker.svc.cluster.local:1026")
        .send(
            Method::GET,
            "@evil.example/x",
            HeaderMap::new(),
            Body::empty(),
        )
        .await
        .expect_err("a path that is not absolute is refused");
    let problem = ProblemDetails::from(error);
    assert_eq!(problem.status, 502);
    let said = serde_json::to_string(&problem).expect("the body");
    assert!(!said.contains("cluster.local"), "{said}");
    assert!(!said.contains("evil.example"), "{said}");
}

#[tokio::test]
async fn an_unusable_upstream_uri_is_also_a_502_that_says_nothing_about_the_upstream() {
    let error = Broker::new("http://broker.jc-context-broker.svc.cluster.local:1026")
        .send(
            Method::GET,
            "http://evil.example/x",
            HeaderMap::new(),
            Body::empty(),
        )
        .await
        .expect_err("no legal uri");
    let problem = ProblemDetails::from(error);
    assert_eq!(problem.status, 502);
    let said = serde_json::to_string(&problem).expect("the body");
    assert!(!said.contains("cluster.local"), "{said}");
    assert!(!said.contains("evil.example"), "{said}");
}

#[tokio::test]
async fn a_second_origin_is_a_second_target_and_the_broker_keeps_its_own() {
    // R46: the egress dispatcher aims the same client at a subscriber's webhook. The broker hop
    // must keep pointing at the broker, or one delivery would redirect every later request.
    let (broker_base, broker_log) = upstream(200, "[]", Vec::new()).await;
    let (sink_base, sink_log) = upstream(204, "", Vec::new()).await;
    let broker = Broker::new(broker_base.clone());
    let aimed = broker.aimed_at(sink_base.clone());

    assert_eq!(broker.base(), broker_base);
    assert_eq!(aimed.base(), sink_base);

    aimed
        .send(Method::POST, "/hook", HeaderMap::new(), Body::from("{}"))
        .await
        .expect("the sink answered");
    broker
        .send(
            Method::GET,
            "/ngsi-ld/v1/entities",
            HeaderMap::new(),
            Body::empty(),
        )
        .await
        .expect("the broker answered");

    assert_eq!(sink_log.lock().expect("the log").len(), 1);
    assert_eq!(broker_log.lock().expect("the log").len(), 1);
    assert_eq!(
        broker_log.lock().expect("the log")[0].uri,
        "/ngsi-ld/v1/entities"
    );
}

#[tokio::test]
async fn an_authorization_header_is_relayed_because_the_hop_is_the_brokers_own() {
    // Struck as an attack, recorded as behaviour: `send` is not where the caller's token is
    // removed. The handlers build the header map they hand in (GW20 strips the tenancy headers
    // there, `edge_gateway_tenancy_strip_tests.rs`), and the broker hop carries what is left.
    let (base, log) = upstream(200, "[]", Vec::new()).await;
    let mut given = HeaderMap::new();
    given.insert(
        AUTHORIZATION,
        HeaderValue::from_static("Bearer broker-token"),
    );
    Broker::new(base)
        .send(Method::GET, "/ngsi-ld/v1/entities", given, Body::empty())
        .await
        .expect("answered");
    let asked = log
        .lock()
        .expect("the log")
        .last()
        .cloned()
        .expect("a request");
    assert_eq!(asked.header("authorization"), vec!["Bearer broker-token"]);
}

#[test]
fn a_header_carrying_a_line_break_cannot_be_built_at_all() {
    // Struck as impossible here rather than left untested: `HeaderValue` refuses CR and LF, so a
    // response splitting payload never reaches `send` to be relayed. The same for the name.
    assert!(HeaderValue::from_str("a\r\nX-Injected: 1").is_err());
    assert!(HeaderValue::from_bytes(b"a\nb").is_err());
    assert!(HeaderName::try_from("x-a\r\nb").is_err());
    let mut map = HeaderMap::new();
    map.insert(CONNECTION, HeaderValue::from_static("close"));
    assert_eq!(map.len(), 1);
    assert!(map.get(HOST).is_none());
}
