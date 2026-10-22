//! A write through MCP passes the guards the same write passes at `/ngsi-ld/v1` (T-4590).
//!
//! A tool call is the NGSI-LD request it stands for: a subscription's notification target is
//! checked, a form mints the id and counts the create, and each signed-in caller spends their own
//! share of a form's creates on every surface, however many share one address. A header that is
//! not a bearer token makes nobody signed in: such a caller gets the anonymous tool set and the
//! anonymous allowance.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::response::IntoResponse;
use axum::Router;
use context_gateway::app::{router, Gateway};
use context_gateway::auth::accounts::ServiceAccounts;
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::Endpoint;
use jc_core::kinds::{Audience, Creates, PolicySpec, RateLimits, Representation};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

const SLUG: &str = "k4y7pq2mzt6vhx3nbwrs5cjd8f";
const SPACE: &str = "ovzdusie";
const DOMAIN: &str = "banskabystrica.sk";
const ADDRESS: &str = "203.0.113.9";

/// The bodies the broker received, in order.
type Hops = Arc<Mutex<Vec<Value>>>;

/// A broker that records every hop and answers each with `status` and `body`.
async fn broker(status: StatusCode, body: Value) -> (String, Hops) {
    let hops: Hops = Arc::default();
    let recorder = Arc::clone(&hops);
    let app = Router::new().fallback(move |request: Request<Body>| {
        let (recorder, body) = (Arc::clone(&recorder), body.clone());
        async move {
            let sent = axum::body::to_bytes(request.into_body(), usize::MAX)
                .await
                .expect("a body");
            recorder
                .lock()
                .expect("the hop log")
                .push(serde_json::from_slice(&sent).unwrap_or(Value::Null));
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

fn grant(role: &str) -> PolicySpec {
    serde_norway::from_str(&format!(
        "contextSpaceRef: {SPACE}\nassigner: did:web:{DOMAIN}\nassignee: {{ kind: role, id: {role} }}\n\
         operations: [queryEntity, retrieveEntity, upsertBatch, createEntity, createSubscription]\n\
         information:\n  - entities: [{{ type: AirQualityObserved }}]\n"
    ))
    .expect("the policy spec parses")
}

/// A public Endpoint whose Policy grants a steward entity and subscription writes; `creates`
/// makes it a form.
fn endpoint(creates: Option<Creates>) -> Endpoint {
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
        creates,
        file_limits: None,
        hidden_attributes: Default::default(),
        projection: None,
        models: Vec::new(),
        view_mapping: None,
        catalog: None,
        deprecation: None,
        feeds: Vec::new(),
        policy_names: Vec::new(),
        manifest_name: String::new(),
        policies: vec![grant("public"), grant("steward")],
    }
}

fn gateway(upstream: &str, endpoint: Endpoint, realm: &common::Realm) -> Gateway {
    Gateway::new(Broker::new(upstream), Box::new(PolicyPdp), DOMAIN)
        .serve([endpoint])
        .authenticate(Arc::new(realm.verifier()), ServiceAccounts::new(), None)
}

fn steward(realm: &common::Realm, name: &str) -> String {
    realm.mint(&json!({
        "iss": common::ISSUER,
        "sub": format!("sub-{name}"),
        "aud": SLUG,
        "preferred_username": name,
        "realm_access": { "roles": ["steward"] },
        "exp": common::in_seconds(300),
        "iat": common::in_seconds(-10),
    }))
}

/// One request from `ADDRESS` with `authorization` as given, the body as JSON.
async fn send(
    app: &Router,
    path: &str,
    authorization: Option<&str>,
    body: &Value,
) -> (StatusCode, Value) {
    let mut request = Request::post(path)
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .header("x-forwarded-for", ADDRESS);
    if let Some(authorization) = authorization {
        request = request.header("authorization", authorization);
    }
    let response = app
        .clone()
        .oneshot(
            request
                .body(Body::from(body.to_string()))
                .expect("a request"),
        )
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

fn mcp() -> String {
    format!("/api/endpoint/{SLUG}/mcp")
}

fn subscription(uri: &str) -> Value {
    json!({
        "type": "Subscription",
        "entities": [{ "type": "AirQualityObserved" }],
        "notification": { "endpoint": { "uri": uri } }
    })
}

/// The tool error's text, or nothing when the call was not refused as a tool error.
fn refusal(answer: &Value) -> Option<String> {
    (answer["result"]["isError"] == true)
        .then(|| answer["result"]["content"][0]["text"].to_string())
}

#[tokio::test]
async fn a_subscription_to_an_internal_address_is_refused_through_mcp_as_through_ngsi_ld() {
    let realm = common::Realm::new();
    let ok = json!({ "jsonrpc": "2.0", "id": 7, "result": { "content": [] } });
    let (upstream, hops) = broker(StatusCode::OK, ok).await;
    let app = router(Arc::new(gateway(&upstream, endpoint(None), &realm)));
    let bearer = format!("Bearer {}", steward(&realm, "jana"));
    let internal = "http://10.43.0.10:8080/";

    let (status, _) = send(
        &app,
        &format!("/api/endpoint/{SLUG}/ngsi-ld/v1/subscriptions"),
        Some(&bearer),
        &subscription(internal),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "the NGSI-LD surface refuses it"
    );

    let (status, answer) = send(
        &app,
        &mcp(),
        Some(&bearer),
        &call(
            "create_subscription",
            json!({ "subscription": subscription(internal) }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    let why = refusal(&answer).unwrap_or_else(|| panic!("not refused: {answer}"));
    assert!(why.contains("inside the platform's own networks"), "{why}");
    let update = call(
        "update_subscription",
        json!({ "id": "urn:ngsi-ld:Subscription:1", "fragment": { "notification": { "endpoint": { "uri": "http://127.0.0.1/" } } } }),
    );
    let (_, answer) = send(&app, &mcp(), Some(&bearer), &update).await;
    assert!(refusal(&answer).is_some(), "{answer}");
    assert!(
        hops.lock().expect("the hop log").is_empty(),
        "a refused write reached the broker"
    );

    // A subscriber on the internet is stored as before.
    let public = call(
        "create_subscription",
        json!({ "subscription": subscription("https://hooks.example.org/air") }),
    );
    let (_, answer) = send(&app, &mcp(), Some(&bearer), &public).await;
    assert!(refusal(&answer).is_none(), "{answer}");
    assert_eq!(hops.lock().expect("the hop log").len(), 1);
}

#[tokio::test]
async fn a_forms_create_through_mcp_gets_a_minted_id_and_one_batch_is_refused() {
    let realm = common::Realm::new();
    let ok = json!({ "jsonrpc": "2.0", "id": 7, "result": { "content": [] } });
    let (upstream, hops) = broker(StatusCode::OK, ok).await;
    let form = Creates {
        mint_ids: true,
        per_day: None,
        embed_origins: Vec::new(),
    };
    let app = router(Arc::new(gateway(&upstream, endpoint(Some(form)), &realm)));
    let bearer = format!("Bearer {}", steward(&realm, "jana"));
    let chosen =
        json!({ "id": "urn:ngsi-ld:AirQualityObserved:mine", "type": "AirQualityObserved" });

    let (_, answer) = send(
        &app,
        &mcp(),
        Some(&bearer),
        &call("create_entity", json!({ "entity": chosen })),
    )
    .await;
    assert!(refusal(&answer).is_none(), "{answer}");
    let sent = hops.lock().expect("the hop log")[0].clone();
    let id = sent["params"]["arguments"]["entity"]["id"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    assert!(
        id.starts_with("urn:ngsi-ld:AirQualityObserved:") && !id.ends_with(":mine"),
        "{sent}"
    );

    let (_, answer) = send(
        &app,
        &mcp(),
        Some(&bearer),
        &call("batch_upsert", json!({ "entities": [chosen] })),
    )
    .await;
    let why = refusal(&answer).unwrap_or_else(|| panic!("not refused: {answer}"));
    assert!(why.contains("one entity per create"), "{why}");
    assert_eq!(hops.lock().expect("the hop log").len(), 1);
}

#[tokio::test]
async fn two_signed_in_callers_behind_one_address_keep_their_own_share_of_a_forms_creates() {
    let realm = common::Realm::new();
    // Ten a day is one per sender.
    let form = || Creates {
        mint_ids: false,
        per_day: Some(10),
        embed_origins: Vec::new(),
    };
    let entity = json!({ "id": "urn:ngsi-ld:AirQualityObserved:1", "type": "AirQualityObserved" });
    let (jana, peter) = (steward(&realm, "jana"), steward(&realm, "peter"));

    let (upstream, _) = broker(StatusCode::CREATED, Value::Null).await;
    let app = router(Arc::new(gateway(&upstream, endpoint(Some(form())), &realm)));
    let entities = format!("/api/endpoint/{SLUG}/ngsi-ld/v1/entities");
    for (token, want) in [(&jana, 201), (&peter, 201), (&jana, 429)] {
        let (status, answer) =
            send(&app, &entities, Some(&format!("Bearer {token}")), &entity).await;
        assert_eq!(status.as_u16(), want, "{answer}");
    }

    let ok = json!({ "jsonrpc": "2.0", "id": 7, "result": { "content": [] } });
    let (upstream, hops) = broker(StatusCode::OK, ok).await;
    let app = router(Arc::new(gateway(&upstream, endpoint(Some(form())), &realm)));
    let create = call("create_entity", json!({ "entity": entity }));
    for (token, refused) in [(&jana, false), (&peter, false), (&jana, true)] {
        let (_, answer) = send(&app, &mcp(), Some(&format!("Bearer {token}")), &create).await;
        assert_eq!(refusal(&answer).is_some(), refused, "{answer}");
    }
    assert_eq!(hops.lock().expect("the hop log").len(), 2);
}

#[tokio::test]
async fn a_header_that_is_not_a_bearer_token_is_anonymous_on_an_endpoints_mcp() {
    let realm = common::Realm::new();
    let tools = json!({ "jsonrpc": "2.0", "id": 1, "result": { "tools": [
        { "name": "query_entities", "inputSchema": { "type": "object" } },
        { "name": "create_subscription", "inputSchema": { "type": "object" } },
    ] } });
    let (upstream, hops) = broker(StatusCode::OK, tools).await;
    let app = router(Arc::new(
        gateway(&upstream, endpoint(None), &realm).limit_anonymous_mcp_to(RateLimits {
            requests_per_minute: 2,
            burst: Some(2),
        }),
    ));
    let list = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" });
    let (status, answer) = send(&app, &mcp(), Some("Basic eDp5"), &list).await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    let names: Vec<&str> = answer["result"]["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("no tool list: {answer}"))
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    assert!(
        names.contains(&"query_entities") && !names.contains(&"create_subscription"),
        "{names:?}"
    );

    let write = call(
        "create_subscription",
        json!({ "subscription": subscription("https://hooks.example.org/") }),
    );
    let (_, answer) = send(&app, &mcp(), Some("Basic b3RoZXI6eA=="), &write).await;
    assert_eq!(answer["error"]["code"], json!(-32602), "{answer}");
    assert_eq!(
        hops.lock().expect("the hop log").len(),
        1,
        "only the list reached the broker"
    );

    // Two calls spent the address's anonymous allowance, whatever each one's Basic header said.
    let (status, _) = send(&app, &mcp(), Some("Basic dGhpcmQ6eA=="), &list).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn a_fresh_basic_header_per_request_buys_no_fresh_rate_limit_bucket() {
    let realm = common::Realm::new();
    let (upstream, _) = broker(StatusCode::OK, json!([])).await;
    let mut limited = endpoint(None);
    limited.rate_limit = Some(RateLimits {
        requests_per_minute: 1,
        burst: Some(1),
    });
    let app = router(Arc::new(gateway(&upstream, limited, &realm)));
    let read = |n: usize| {
        Request::get(format!(
            "/api/endpoint/{SLUG}/ngsi-ld/v1/entities?type=AirQualityObserved"
        ))
        .header("x-forwarded-for", ADDRESS)
        .header("authorization", format!("Basic {n}"))
        .body(Body::empty())
        .expect("a request")
    };
    let first = app.clone().oneshot(read(1)).await.expect("an answer");
    assert_ne!(first.status(), StatusCode::TOO_MANY_REQUESTS);
    let second = app.clone().oneshot(read(2)).await.expect("an answer");
    assert_eq!(second.status(), StatusCode::TOO_MANY_REQUESTS);
}
