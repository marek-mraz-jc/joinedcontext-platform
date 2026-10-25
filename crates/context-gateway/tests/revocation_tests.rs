//! A subject the Portal revoked is refused on every door within one poll of its list, not at
//! the expiry of the token it still holds (T-5568, TR-03187 AR-6).

mod common;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use axum::Router;
use context_gateway::app::{router, Gateway};
use context_gateway::auth::accounts::ServiceAccounts;
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::{Endpoint, Model};
use context_gateway::revocations::Revocations;
use jc_core::kinds::{Audience, PolicySpec, Representation};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

const AIR: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaa";

fn endpoint() -> Endpoint {
    let policy: PolicySpec = serde_norway::from_str(
        "contextSpaceRef: ovzdusie\n\
         assigner: did:web:banskabystrica.sk\n\
         assignee: { kind: role, id: steward }\n\
         operations: [queryEntity]\n\
         information:\n  \
         - entities:\n      \
         - type: AirQualityObserved\n",
    )
    .expect("the policy parses");
    Endpoint {
        declared_types: None,
        roles: Default::default(),
        slug: AIR.to_owned(),
        title: [("en".to_owned(), "Air".to_owned())].into_iter().collect(),
        description: Default::default(),
        space: "ovzdusie".to_owned(),
        project: "ovzdusie".to_owned(),
        audience: Audience::Organization,
        allowed_projects: Vec::new(),
        representations: vec![Representation::NgsiLd, Representation::Mcp],
        rate_limit: None,
        creates: None,
        file_limits: None,
        hidden_attributes: Default::default(),
        projection: None,
        view_mapping: None,
        catalog: None,
        deprecation: None,
        feeds: Vec::new(),
        calendars: Vec::new(),
        map_defaults: Default::default(),
        base_path: format!("/api/endpoint/{AIR}"),
        models: vec![Model {
            name: "air-quality".to_owned(),
            version: "1.0.0".to_owned(),
            major: 1,
            classes: vec!["AirQualityObserved".to_owned()],
            json_schema: None,
            context: None,
        }],
        policy_names: Vec::new(),
        manifest_name: String::new(),
        policies: vec![policy],
    }
}

fn steward(realm: &common::Realm, sub: &str) -> String {
    realm.mint(&json!({
        "iss": common::ISSUER,
        "sub": sub,
        "aud": ["context-gateway", AIR],
        "azp": "edge",
        "preferred_username": format!("jana-{sub}"),
        "groups": ["/ovzdusie"],
        "realm_access": { "roles": ["steward"] },
        "exp": common::in_seconds(300),
        "iat": common::in_seconds(-10),
    }))
}

fn request(method: Method, path: &str, token: &str, body: Option<Value>) -> Request<Body> {
    let builder = Request::builder()
        .method(method)
        .uri(path)
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json");
    builder
        .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
        .expect("a request")
}

/// The status of the REST door, the hub MCP and the Endpoint MCP for `token`.
async fn doors(app: &Router, token: &str) -> [StatusCode; 3] {
    let list = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" });
    let calls = [
        request(
            Method::GET,
            &format!("/api/endpoint/{AIR}/ngsi-ld/v1/entities?type=AirQualityObserved"),
            token,
            None,
        ),
        request(Method::POST, "/api/mcp", token, Some(list.clone())),
        request(
            Method::POST,
            &format!("/api/endpoint/{AIR}/mcp"),
            token,
            Some(list),
        ),
    ];
    let mut statuses = [StatusCode::OK; 3];
    for (status, call) in statuses.iter_mut().zip(calls) {
        *status = app
            .clone()
            .oneshot(call)
            .await
            .expect("the gateway answers")
            .status();
    }
    statuses
}

/// The Portal's internal list, whatever `items` holds at the moment it is asked.
async fn portal(items: Arc<Mutex<Value>>) -> String {
    let app = Router::new().route(
        "/internal/revocations",
        axum::routing::get(move || {
            let items = Arc::clone(&items);
            async move {
                let items = items.lock().map(|i| i.clone()).unwrap_or(Value::Null);
                axum::Json(json!({ "items": items }))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a port");
    let address = listener.local_addr().expect("an address");
    tokio::spawn(async move { axum::serve(listener, app).await });
    format!("http://{address}/internal/revocations")
}

#[tokio::test]
async fn a_revoked_subject_is_refused_on_rest_and_mcp_within_one_poll() {
    let realm = common::Realm::new();
    let broker = common::BrokerStub::start(vec![json!([])]).await;
    let verifier = Arc::new(realm.verifier());
    let gateway = Gateway::new(
        Broker::new(&broker.url),
        Box::new(PolicyPdp),
        "banskabystrica.sk",
    )
    .serve(vec![endpoint()])
    .authenticate(
        Arc::clone(&verifier),
        ServiceAccounts::new(),
        Some("https://city.example".to_owned()),
    );
    let app = router(Arc::new(gateway));
    let jana = steward(&realm, "0f5a");
    let petra = steward(&realm, "1b2c");

    for status in doors(&app, &jana).await {
        assert_ne!(
            status,
            StatusCode::UNAUTHORIZED,
            "admitted before the revocation"
        );
    }

    let items = Arc::new(Mutex::new(json!([])));
    let mut poller =
        Revocations::new(portal(Arc::clone(&items)).await, None, verifier).expect("a poller");
    assert_eq!(poller.poll().await, Ok(Some(0)));

    // Disabled now: every token issued up to this second is refused at the next poll.
    *items.lock().expect("the list") =
        json!([{ "subject": "0f5a", "revokedAt": common::in_seconds(0) }]);
    assert_eq!(poller.poll().await, Ok(Some(1)));
    assert_eq!(doors(&app, &jana).await, [StatusCode::UNAUTHORIZED; 3]);
    for status in doors(&app, &petra).await {
        assert_ne!(status, StatusCode::UNAUTHORIZED, "another person stays in");
    }

    // A Portal that stops answering leaves the list as it was.
    drop(items.lock().map(|mut i| *i = Value::Null));
    assert!(poller.poll().await.is_err());
    assert_eq!(doors(&app, &jana).await, [StatusCode::UNAUTHORIZED; 3]);
}
