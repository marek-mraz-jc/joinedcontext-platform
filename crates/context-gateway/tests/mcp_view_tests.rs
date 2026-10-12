//! The MCP surface of an Endpoint and of a space is the broker's MCP surface of its view
//! (T-3647, plan 5.3, map 7.2).
//!
//! `POST /api/endpoint/{slug}/mcp` goes to `/x/views/{slug}/mcp` and `POST /cs/{space}/mcp` to
//! `/x/views/cs-{space}/mcp`, the message as sent but for the two caps, the tenant and the
//! verified caller pinned. The broker's tools and their confirmation are the broker's; the
//! gateway still answers `describe_access` and `describe_schema` (no broker tools, Antares
//! `mcp-surface.md` section 9) and what no view serves yet.

mod common;

use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use axum::response::IntoResponse;
use axum::Router;
use context_gateway::app::{router, Gateway};
use context_gateway::auth::accounts::ServiceAccounts;
use context_gateway::domain_gate::{DomainGate, Mode};
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::{Endpoint, Space};
use context_gateway::translators::view_mapping::ViewMapping;
use jc_core::kinds::{Audience, PolicySpec, RateLimits, Representation};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

const SLUG: &str = "k4y7pq2mzt6vhx3nbwrs5cjd8f";
const SPACE: &str = "ovzdusie";
const DOMAIN: &str = "banskabystrica.sk";

/// One hop as the broker received it.
#[derive(Clone)]
struct Hop {
    path: String,
    headers: HeaderMap,
    body: Value,
}

type Hops = Arc<Mutex<Vec<Hop>>>;

/// A broker that records every hop and answers each with `status` and `body`.
async fn broker(status: StatusCode, body: Value) -> (String, Hops) {
    let hops: Hops = Arc::default();
    let recorder = Arc::clone(&hops);
    let app = Router::new().fallback(move |request: Request<Body>| {
        let (recorder, body) = (Arc::clone(&recorder), body.clone());
        async move {
            let (parts, sent) = request.into_parts();
            let sent = axum::body::to_bytes(sent, usize::MAX)
                .await
                .expect("a body");
            recorder.lock().expect("the hop log").push(Hop {
                path: parts.uri.path().to_owned(),
                headers: parts.headers,
                body: serde_json::from_slice(&sent).unwrap_or(Value::Null),
            });
            (status, axum::Json(body)).into_response()
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a free port");
    let url = format!("http://{}", listener.local_addr().expect("an address"));
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (url, hops)
}

fn grant() -> PolicySpec {
    serde_norway::from_str(&format!(
        "contextSpaceRef: {SPACE}\nassigner: did:web:{DOMAIN}\nassignee: {{ kind: role, id: public }}\n\
         operations: [queryEntity, retrieveEntity, retrieveEntityTypes, upsertBatch]\n\
         information:\n  - entities: [{{ type: AirQualityObserved }}]\n"
    ))
    .expect("the policy spec parses")
}

fn endpoint() -> Endpoint {
    Endpoint {
        declared_types: None,
        roles: Default::default(),
        slug: SLUG.to_owned(),
        title: Default::default(),
        description: Default::default(),
        base_path: format!("/api/endpoint/{SLUG}"),
        space: SPACE.to_owned(),
        project: SPACE.to_owned(),
        audience: Audience::Public,
        allowed_projects: Vec::new(),
        representations: vec![Representation::NgsiLd, Representation::Mcp],
        rate_limit: None,
        creates: None,
        file_limits: None,
        hidden_attributes: Default::default(),
        projection: None,
        models: Vec::new(),
        view_mapping: None,
        catalog: None,
        policy_names: Vec::new(),
        policies: vec![grant()],
    }
}

fn gateway(upstream: String, endpoint: Endpoint) -> Gateway {
    let space = Space {
        endpoint: Arc::new(Endpoint {
            slug: SPACE.to_owned(),
            base_path: format!("/cs/{SPACE}"),
            ..endpoint.clone()
        }),
        title: Default::default(),
        description: Default::default(),
        is_sandbox: false,
        default_locale: None,
    };
    Gateway::new(Broker::new(upstream), Box::new(PolicyPdp), DOMAIN)
        .serve([endpoint])
        .serve_spaces([space])
}

async fn post(gateway: Gateway, path: &str, message: &Value) -> (StatusCode, Value) {
    let request = Request::post(path)
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .header("cookie", "session=s3cret")
        .body(Body::from(message.to_string()))
        .expect("a request");
    let response = router(Arc::new(gateway))
        .oneshot(request)
        .await
        .expect("the gateway answers");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("a body");
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn call(tool: &str, arguments: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": 7,
        "method": "tools/call",
        "params": { "name": tool, "arguments": arguments }
    })
}

fn endpoint_mcp() -> String {
    format!("/api/endpoint/{SLUG}/mcp")
}

fn tool_names(answer: &Value) -> Vec<String> {
    answer["result"]["tools"]
        .as_array()
        .map(|tools| {
            tools
                .iter()
                .filter_map(|tool| tool["name"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// A tool call reaches the broker's MCP surface of the Endpoint's view, under the tenant and
/// the verified caller, without the caller's cookie; the broker's answer comes back as given.
#[tokio::test]
async fn a_tool_call_is_the_brokers_answer_under_the_verified_caller() {
    let answered =
        json!({ "jsonrpc": "2.0", "id": 7, "result": { "structuredContent": { "entities": [] } } });
    let (upstream, hops) = broker(StatusCode::OK, answered.clone()).await;
    let message = call("query_entities", json!({ "type": ["AirQualityObserved"] }));
    let (status, answer) = post(gateway(upstream, endpoint()), &endpoint_mcp(), &message).await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(answer, answered);

    let hops = hops.lock().expect("the hop log").clone();
    assert_eq!(hops.len(), 1);
    let hop = &hops[0];
    assert_eq!(hop.path, format!("/x/views/{SLUG}/mcp"));
    assert_eq!(hop.body, message);
    let header = |name: &str| hop.headers.get(name).and_then(|v| v.to_str().ok());
    assert_eq!(header("ngsild-tenant"), Some(SPACE));
    assert_eq!(header("x-jc-roles"), Some("public"));
    assert_eq!(header("cookie"), None);
    assert_eq!(header("authorization"), None);
}

/// The space surface is the space's view (`cs-{space}`).
#[tokio::test]
async fn the_space_surface_is_the_spaces_view() {
    let (upstream, hops) = broker(
        StatusCode::OK,
        json!({ "jsonrpc": "2.0", "id": 7, "result": {} }),
    )
    .await;
    let message = call(
        "get_entity",
        json!({ "id": "urn:ngsi-ld:AirQualityObserved:s1" }),
    );
    let (status, _) = post(
        gateway(upstream, endpoint()),
        &format!("/cs/{SPACE}/mcp"),
        &message,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let hops = hops.lock().expect("the hop log").clone();
    assert_eq!(hops.len(), 1);
    assert_eq!(hops[0].path, format!("/x/views/cs-{SPACE}/mcp"));
}

/// The broker lists its tools; the gateway adds the two that describe the Endpoint, as the
/// caller's grant shows them, and answers those itself without a hop.
#[tokio::test]
async fn the_listing_is_the_brokers_plus_the_two_describe_tools_the_gateway_answers() {
    let listed = json!({ "jsonrpc": "2.0", "id": 1, "result": { "tools": [
        { "name": "query_entities", "inputSchema": { "type": "object" } }
    ] } });
    let (upstream, hops) = broker(StatusCode::OK, listed).await;
    let list = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" });
    let (status, answer) = post(
        gateway(upstream.clone(), endpoint()),
        &endpoint_mcp(),
        &list,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(
        tool_names(&answer),
        ["query_entities", "describe_access", "describe_schema"],
        "{answer}"
    );
    assert_eq!(hops.lock().expect("the hop log").len(), 1);

    for tool in ["describe_access", "describe_schema"] {
        let (status, answer) = post(
            gateway(upstream.clone(), endpoint()),
            &endpoint_mcp(),
            &call(tool, json!({})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{tool}: {answer}");
        assert!(answer["result"].is_object(), "{tool}: {answer}");
    }
    assert_eq!(
        hops.lock().expect("the hop log").len(),
        1,
        "the describe tools never reach the broker"
    );
}

/// The two costs one call may put on the shared broker are capped as on the NGSI-LD surface:
/// `joinLevel` (AG-25) and `lastN` (GW26).
#[tokio::test]
async fn join_level_and_last_n_are_capped_in_the_arguments() {
    let (upstream, hops) = broker(
        StatusCode::OK,
        json!({ "jsonrpc": "2.0", "id": 7, "result": {} }),
    )
    .await;
    for message in [
        call(
            "get_entity",
            json!({ "id": "urn:ngsi-ld:A:1", "joinLevel": 9 }),
        ),
        call("query_temporal", json!({ "type": ["A"], "lastN": 5000 })),
        call(
            "get_entity",
            json!({ "id": "urn:ngsi-ld:A:1", "joinLevel": 2 }),
        ),
    ] {
        post(
            gateway(upstream.clone(), endpoint()),
            &endpoint_mcp(),
            &message,
        )
        .await;
    }
    let hops = hops.lock().expect("the hop log").clone();
    assert_eq!(hops[0].body["params"]["arguments"]["joinLevel"], json!(3));
    assert_eq!(hops[1].body["params"]["arguments"]["lastN"], json!(1000));
    assert_eq!(hops[2].body["params"]["arguments"]["joinLevel"], json!(2));
}

/// A broker failure is the gateway's own 5xx, never the broker's wording.
#[tokio::test]
async fn a_broker_failure_is_masked() {
    let (upstream, _) = broker(
        StatusCode::INTERNAL_SERVER_ERROR,
        json!({ "detail": "tenant ovzdusie: pool exhausted at 10.0.0.7" }),
    )
    .await;
    let message = call("query_entities", json!({ "type": ["AirQualityObserved"] }));
    let (status, answer) = post(gateway(upstream, endpoint()), &endpoint_mcp(), &message).await;
    assert!(status.is_server_error(), "{status}");
    assert!(!answer.to_string().contains("10.0.0.7"), "{answer}");
}

/// PF-41: a write tool to a space whose domain is not verified is refused before the broker
/// is asked; a read is not.
#[tokio::test]
async fn a_write_tool_meets_the_domain_gate_before_the_broker() {
    let (upstream, hops) = broker(
        StatusCode::OK,
        json!({ "jsonrpc": "2.0", "id": 7, "result": {} }),
    )
    .await;
    let enforced = || {
        gateway(upstream.clone(), endpoint())
            .gate_writes_on(Arc::new(DomainGate::new(Mode::Enforce)))
    };
    let write = call("batch_upsert", json!({ "entities": [] }));
    let (status, _) = post(enforced(), &endpoint_mcp(), &write).await;
    assert!(status.is_client_error(), "{status}");
    assert!(hops.lock().expect("the hop log").is_empty());

    let read = call("query_entities", json!({ "type": ["AirQualityObserved"] }));
    let (status, _) = post(enforced(), &endpoint_mcp(), &read).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(hops.lock().expect("the hop log").len(), 1);
}

/// An Endpoint with a view mapping has no view at the broker yet: its MCP stays the gateway's.
#[tokio::test]
async fn a_mapped_endpoint_stays_on_the_gateways_mcp() {
    let (upstream, hops) = broker(StatusCode::OK, json!({})).await;
    let mapping = ViewMapping::parse(&json!({
        "version": 2,
        "sourceClass": "Senzor",
        "targetClass": "AirQualityObserved",
        "slots": [{ "target": "pm10", "source": "pm10", "kind": "rename", "filterable": true }]
    }))
    .expect("the IR parses");
    let mapped = Endpoint {
        view_mapping: Some(Arc::new(mapping)),
        ..endpoint()
    };
    let list = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" });
    let (status, answer) = post(gateway(upstream, mapped), &endpoint_mcp(), &list).await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert!(
        tool_names(&answer).contains(&"query_entities".to_owned()),
        "{answer}"
    );
    assert!(hops.lock().expect("the hop log").is_empty());
}

/// AG-25: a tool call spends the Endpoint's quota like any other request; the limiter stands
/// in front of the hop (T-3647 moved the tools, not the quota).
#[tokio::test]
async fn the_mcp_route_is_rate_limited() {
    let (upstream, hops) = broker(
        StatusCode::OK,
        json!({ "jsonrpc": "2.0", "id": 7, "result": {} }),
    )
    .await;
    let limited = Endpoint {
        rate_limit: Some(RateLimits {
            requests_per_minute: 60,
            burst: Some(1),
        }),
        ..endpoint()
    };
    let app = router(Arc::new(gateway(upstream, limited)));
    let message = call("query_entities", json!({ "type": ["AirQualityObserved"] }));
    let send = |app: Router| {
        let message = message.clone();
        async move {
            app.oneshot(
                Request::post(endpoint_mcp())
                    .header("content-type", "application/json")
                    .header("x-forwarded-for", "203.0.113.9")
                    .body(Body::from(message.to_string()))
                    .expect("a request"),
            )
            .await
            .expect("the stack answers")
            .status()
        }
    };
    assert_eq!(send(app.clone()).await, StatusCode::OK);
    assert_eq!(send(app).await, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        hops.lock().expect("the hop log").len(),
        1,
        "the refused call made no hop"
    );
}

/// SP-17, SP-18: a token nobody can verify, one that has expired and one minted for another
/// audience never reach the broker as anyone but the anonymous caller; the steward's own token
/// does, so the refusals are the token being judged (carried over from the façade's
/// `mcp_read_security_tests`, T-3647).
#[tokio::test]
async fn a_token_the_gateway_cannot_accept_never_reaches_the_broker_as_its_holder() {
    let realm = common::Realm::new();
    let claims = |aud: &str, exp: i64| {
        json!({
            "iss": common::ISSUER,
            "sub": "0f5a",
            "aud": aud,
            "preferred_username": "jana",
            "realm_access": { "roles": ["steward"] },
            "exp": common::in_seconds(exp),
            "iat": common::in_seconds(-10),
        })
    };
    let steward = realm.mint(&claims(SLUG, 300));
    let cases = [
        (realm.mint(&claims("another-endpoint", 300)), false),
        (realm.mint(&claims(SLUG, -300)), false),
        ("not.a.token".to_owned(), false),
        (steward, true),
    ];
    for (token, holder) in cases {
        let (upstream, hops) = broker(
            StatusCode::OK,
            json!({ "jsonrpc": "2.0", "id": 7, "result": {} }),
        )
        .await;
        let gateway = gateway(upstream, endpoint()).authenticate(
            Arc::new(realm.verifier()),
            ServiceAccounts::new(),
            None,
        );
        let request = Request::post(endpoint_mcp())
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {token}"))
            .body(Body::from(
                call("query_entities", json!({ "type": ["AirQualityObserved"] })).to_string(),
            ))
            .expect("a request");
        let status = router(Arc::new(gateway))
            .oneshot(request)
            .await
            .expect("the gateway answers")
            .status();
        let hops = hops.lock().expect("the hop log").clone();
        let roles: Vec<String> = hops
            .iter()
            .filter_map(|hop| hop.headers.get("x-jc-roles"))
            .filter_map(|value| value.to_str().ok().map(str::to_owned))
            .collect();
        let as_steward = roles.iter().any(|roles| roles.contains("steward"));
        assert_eq!(as_steward, holder, "{status}: roles {roles:?}");
        if !holder {
            assert!(
                status == StatusCode::UNAUTHORIZED || !hops.is_empty(),
                "refused, or read as anonymous: {status}"
            );
        }
    }
}

/// The broker lists its view's resources; the gateway adds the `schema://` and `access://`
/// documents behind the two describe tools and reads them itself, so every URI it advertises
/// answers (EP-52) and none of them is asked of the broker.
#[tokio::test]
async fn the_resource_listing_carries_the_gateways_documents_and_reads_them_without_a_hop() {
    let listed = json!({ "jsonrpc": "2.0", "id": 1, "result": { "resources": [
        { "uri": format!("ngsi-ld://{SLUG}/types/AirQualityObserved"), "name": "AirQualityObserved" }
    ] } });
    let (upstream, hops) = broker(StatusCode::OK, listed).await;
    let list = json!({ "jsonrpc": "2.0", "id": 1, "method": "resources/list" });
    let (status, answer) = post(
        gateway(upstream.clone(), endpoint()),
        &endpoint_mcp(),
        &list,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    let uris: Vec<&str> = answer["result"]["resources"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|resource| resource["uri"].as_str())
        .collect();
    assert!(
        uris.iter().any(|uri| uri.starts_with("ngsi-ld://")),
        "{answer}"
    );
    let access = uris
        .iter()
        .find(|uri| uri.starts_with("access://"))
        .unwrap_or_else(|| panic!("the access document is listed: {answer}"))
        .to_string();
    assert_eq!(hops.lock().expect("the hop log").len(), 1);

    let read = json!({ "jsonrpc": "2.0", "id": 2, "method": "resources/read", "params": { "uri": access } });
    let (status, answer) = post(gateway(upstream, endpoint()), &endpoint_mcp(), &read).await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert!(answer["result"]["contents"].is_array(), "{answer}");
    assert_eq!(
        hops.lock().expect("the hop log").len(),
        1,
        "read without a hop"
    );
}

/// The broker's MCP answers 406 unless the request accepts both JSON and an event stream
/// (Streamable HTTP); the façade took `application/json` alone, and the Portal's agents send
/// just that. The hop accepts both, whatever the caller sent.
#[tokio::test]
async fn the_hop_accepts_what_the_brokers_transport_asks_for() {
    for accept in [Some("application/json"), Some("*/*"), None] {
        let (upstream, hops) = broker(
            StatusCode::OK,
            json!({ "jsonrpc": "2.0", "id": 7, "result": {} }),
        )
        .await;
        let mut request = Request::post(endpoint_mcp()).header("content-type", "application/json");
        if let Some(accept) = accept {
            request = request.header("accept", accept);
        }
        let request = request
            .body(Body::from(
                call("query_entities", json!({ "type": ["AirQualityObserved"] })).to_string(),
            ))
            .expect("a request");
        let status = router(Arc::new(gateway(upstream, endpoint())))
            .oneshot(request)
            .await
            .expect("the gateway answers")
            .status();
        assert_eq!(status, StatusCode::OK, "{accept:?}");
        let hops = hops.lock().expect("the hop log").clone();
        let sent = hops[0]
            .headers
            .get("accept")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        assert!(
            sent.contains("application/json") && sent.contains("text/event-stream"),
            "{accept:?} became {sent:?}"
        );
    }
}

/// What is no single request never reaches the broker: a notification is acknowledged, and a
/// JSON-RPC batch is answered as the façade always answered one (the broker refuses batches
/// whole too, Antares `mcp_transport`).
#[tokio::test]
async fn a_notification_and_a_batch_make_no_hop() {
    let (upstream, hops) = broker(StatusCode::OK, json!({})).await;
    let notification = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
    let request = Request::post(endpoint_mcp())
        .header("content-type", "application/json")
        .body(Body::from(notification.to_string()))
        .expect("a request");
    let status = router(Arc::new(gateway(upstream.clone(), endpoint())))
        .oneshot(request)
        .await
        .expect("the gateway answers")
        .status();
    assert_eq!(status, StatusCode::ACCEPTED);

    let batch = json!([call(
        "query_entities",
        json!({ "type": ["AirQualityObserved"] })
    )]);
    let (status, answer) = post(gateway(upstream, endpoint()), &endpoint_mcp(), &batch).await;
    assert!(
        status == StatusCode::ACCEPTED || status == StatusCode::OK && answer.get("error").is_some(),
        "{status}: {answer}"
    );
    assert!(hops.lock().expect("the hop log").is_empty());
}

/// A listing the broker answers as an event stream is passed on as it came: the gateway adds
/// its own entries only to a JSON answer it can read whole.
#[tokio::test]
async fn an_event_stream_listing_is_passed_on_untouched() {
    let stream =
        "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"tools\":[]}}\n\n";
    let app = Router::new().fallback(move || async move {
        ([("content-type", "text/event-stream")], stream).into_response()
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a free port");
    let upstream = format!("http://{}", listener.local_addr().expect("an address"));
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let list = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" });
    let request = Request::post(endpoint_mcp())
        .header("content-type", "application/json")
        .body(Body::from(list.to_string()))
        .expect("a request");
    let response = router(Arc::new(gateway(upstream, endpoint())))
        .oneshot(request)
        .await
        .expect("the gateway answers");
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("a body");
    assert_eq!(String::from_utf8_lossy(&bytes), stream);
}
