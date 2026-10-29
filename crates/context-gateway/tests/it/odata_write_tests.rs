//! OData writes through an Endpoint pass the guards of the NGSI-LD view forward before the
//! broker's view decides them (EP-104, EP-97, EP-61, T-3939): a form's key is minted, its day's
//! creates are counted, a hidden attribute is never written, and the broker's refusals reach
//! the caller as it answers them.

use crate::common;

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use axum::Router;
use context_gateway::app::{router, Gateway};
use context_gateway::auth::accounts::ServiceAccounts;
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::store;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

const HOST: &str = "https://hel.example.fi";
/// A public form: minted ids, one create a day, `contact` hidden.
const FORM: &str = "ffffffffffffffffffffffffff";
/// A public Endpoint with no write guard of its own.
const OPEN: &str = "oooooooooooooooooooooooooo";

const SPACE: &str = "apiVersion: joinedcontext.com/v1alpha1\nkind: ContextSpace\nmetadata:\n  \
                     name: helsinki\n  namespace: helsinki\nspec:\n  isSandbox: false\n  \
                     urnSegment: helsinki\n";

fn endpoint(name: &str, slug: &str, extra: &str) -> String {
    format!(
        "apiVersion: joinedcontext.com/v1alpha1\nkind: Endpoint\nmetadata:\n  name: {name}\n  \
         namespace: helsinki\nspec:\n  contextSpaceRef: helsinki\n  slug: {slug}\n  \
         audience: public\n{extra}"
    )
}

fn repo(test: &str) -> PathBuf {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after the epoch")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("gw-odata-write-{test}-{now}"));
    std::fs::create_dir_all(&dir).expect("create the repository");
    let write = |name: &str, body: &str| std::fs::write(dir.join(name), body).expect("write");
    write("space.yaml", SPACE);
    write(
        "form.yaml",
        &endpoint(
            "helsinki-feedback",
            FORM,
            "  creates:\n    mintIds: true\n    perDay: 1\n  projection:\n    \
             hiddenAttributes: [contact]\n",
        ),
    );
    write("open.yaml", &endpoint("helsinki-bikes", OPEN, ""));
    write(
        "policy.yaml",
        "apiVersion: joinedcontext.com/v1alpha1\nkind: Policy\nmetadata:\n  name: public\n  \
         namespace: helsinki\nspec:\n  contextSpaceRef: { kind: ContextSpace, name: helsinki }\n  \
         assigner: did:web:hel.fi\n  assignee: { kind: role, id: public }\n  \
         operations: [retrieveOps]\n  information:\n    - entities:\n        - type: Feedback\n",
    );
    dir
}

type Hops = Arc<Mutex<Vec<String>>>;

/// The CSDL JSON of the view: `Feedback` keyed by `id`, `Plate` by its alias `number`.
fn csdl() -> Value {
    json!({
        "$Version": "4.01",
        "$EntityContainer": "jc.Container",
        "jc": {
            "Container": {
                "Feedback": {"$Collection": true, "$Type": "jc.Feedback"},
                "Plate": {"$Collection": true, "$Type": "jc.Plate"}
            },
            "Feedback": {"$Kind": "EntityType", "$Key": ["id"]},
            "Plate": {"$Kind": "EntityType", "$Key": [{"number": "plate/number"}]}
        }
    })
}

/// A broker view: `$metadata` as above, the set `Locked` read-only (405), the set `Refused`
/// refused by the engine (403), `$crossjoin` not served (501), the set `Broken` failing (500),
/// every other write taken. Records `METHOD path body`.
async fn broker() -> (String, Hops) {
    let hops: Hops = Arc::default();
    let recorder = Arc::clone(&hops);
    let app = Router::new().fallback(move |request: Request<Body>| {
        let recorder = Arc::clone(&recorder);
        async move {
            let (parts, body) = request.into_parts();
            let path = parts.uri.path().to_owned();
            let body = axum::body::to_bytes(body, 1024 * 1024)
                .await
                .unwrap_or_default();
            recorder.lock().expect("the hop log").push(format!(
                "{} {path} {}",
                parts.method,
                String::from_utf8_lossy(&body)
            ));
            let answer = |status: u16, body: Value| {
                let mut response = axum::Json(body).into_response();
                *response.status_mut() = StatusCode::from_u16(status).expect("a status");
                response
            };
            use axum::response::IntoResponse;
            if path.ends_with("/$metadata") {
                return answer(200, csdl());
            }
            if path.contains("/Locked") {
                let mut refused = answer(405, json!({"error": {"code": "NotAllowed"}}));
                refused
                    .headers_mut()
                    .insert(header::ALLOW, "GET, HEAD".parse().expect("a header"));
                return refused;
            }
            if path.contains("/$crossjoin") {
                return answer(
                    501,
                    json!({"error": {"code": "NotImplemented", "message": "hunter2"}}),
                );
            }
            if path.contains("/Broken") {
                return answer(
                    500,
                    json!({"error": {"code": "Internal", "message": "hunter2"}}),
                );
            }
            if path.contains("/Refused") {
                return answer(
                    403,
                    json!({"type": "urn:antares:error:AccessDenied", "title": "AccessDenied",
                           "status": 403, "detail": "no grant"}),
                );
            }
            if parts.method == Method::POST {
                return answer(201, json!({"id": "minted-by-the-broker"}));
            }
            answer(204, Value::Null)
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

fn app(dir: &Path, upstream: &str, realm: &common::Realm) -> Router {
    let (endpoints, ..) = store::load(dir).expect("the repository loads");
    router(Arc::new(
        Gateway::new(Broker::new(upstream), Box::new(PolicyPdp), "hel.fi")
            .serve(endpoints)
            .authenticate(
                Arc::new(realm.verifier()),
                ServiceAccounts::new(),
                Some(HOST.to_owned()),
            ),
    ))
}

async fn send(
    app: &Router,
    method: Method,
    slug: &str,
    rest: &str,
    body: Option<Value>,
) -> (StatusCode, axum::http::HeaderMap, Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(format!("/api/endpoint/{slug}/odata/v4{rest}"))
        // a delete names why (DM-113)
        .header("jc-reason", "test");
    let body = match body {
        Some(body) => {
            request = request.header(header::CONTENT_TYPE, "application/json");
            Body::from(body.to_string())
        }
        None => Body::empty(),
    };
    let response = app
        .clone()
        .oneshot(request.body(body).expect("a request"))
        .await
        .expect("the gateway answers");
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("a readable body");
    (
        status,
        headers,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn writes(hops: &Hops) -> Vec<String> {
    hops.lock()
        .expect("the hop log")
        .iter()
        .filter(|hop| !hop.starts_with("GET "))
        .cloned()
        .collect()
}

/// EP-97: a create on a minting Endpoint leaves the key to the broker; one that names it is
/// refused before the broker sees it, an aliased key included, and the day's one create is
/// counted, so the next is 429 with `Retry-After`.
#[tokio::test]
async fn a_form_mints_the_key_and_counts_the_days_creates() {
    let realm = common::Realm::new();
    let (upstream, hops) = broker().await;
    let app = app(&repo("mint"), &upstream, &realm);

    let (status, _, body) = send(
        &app,
        Method::POST,
        FORM,
        "/Feedback",
        Some(json!({"id": "urn:ngsi-ld:Feedback:mine", "text": "hi"})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .is_some_and(|d| d.contains("`id`")),
        "{body}"
    );
    let (status, _, body) = send(
        &app,
        Method::POST,
        FORM,
        "/Plate",
        Some(json!({"number": "ABC-1"})),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "an aliased key is a key too: {body}"
    );
    assert!(
        writes(&hops).is_empty(),
        "nothing chosen reached the broker"
    );

    let (status, _, body) = send(
        &app,
        Method::POST,
        FORM,
        "/Feedback",
        Some(json!({"text": "hi"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(
        writes(&hops),
        vec![format!(
            "POST /x/views/{FORM}/odata/v4/Feedback {{\"text\":\"hi\"}}"
        )]
    );

    let (status, headers, body) = send(
        &app,
        Method::POST,
        FORM,
        "/Feedback",
        Some(json!({"text": "again"})),
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert!(headers.contains_key("retry-after"));
    assert_eq!(
        writes(&hops).len(),
        1,
        "the refused create never reached the broker"
    );
}

/// A create the broker refuses gives its place in the day back.
#[tokio::test]
async fn a_refused_create_does_not_spend_the_day() {
    let realm = common::Realm::new();
    let (upstream, _) = broker().await;
    let app = app(&repo("refund"), &upstream, &realm);
    let (status, ..) = send(&app, Method::POST, FORM, "/Locked", Some(json!({}))).await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    let (status, _, body) = send(
        &app,
        Method::POST,
        FORM,
        "/Feedback",
        Some(json!({"text": "hi"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
}

/// EP-61: a write naming a hidden attribute, in the body, as a bind, as the property of the
/// path or in the broker's escaped spelling, is refused before the broker sees it.
#[tokio::test]
async fn a_hidden_attribute_is_never_written() {
    let realm = common::Realm::new();
    let (upstream, hops) = broker().await;
    let app = app(&repo("hidden"), &upstream, &realm);
    for (method, rest, body) in [
        (
            Method::PATCH,
            "/Feedback('x')",
            Some(json!({"contact": "me@x"})),
        ),
        (
            Method::PUT,
            "/Feedback('x')",
            Some(json!({"text": "t", "contact": "me@x"})),
        ),
        (
            Method::PATCH,
            "/Feedback('x')",
            Some(json!({"contact@odata.bind": "Person('p')"})),
        ),
        (
            Method::PUT,
            "/Feedback('x')/contact",
            Some(json!({"value": "me@x"})),
        ),
        (Method::DELETE, "/Feedback('x')/contact", None),
        (
            Method::PUT,
            "/Feedback('x')/contact/$ref",
            Some(json!({"@odata.id": "Person('p')"})),
        ),
        (
            Method::PATCH,
            "/Feedback('x')",
            Some(json!({"co_x006E_tact": "me@x"})),
        ),
    ] {
        let (status, _, answer) = send(&app, method.clone(), FORM, rest, body).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {rest}: {answer}");
    }
    assert!(writes(&hops).is_empty(), "{:?}", writes(&hops));

    let (status, _, answer) = send(
        &app,
        Method::PATCH,
        FORM,
        "/Feedback('x')",
        Some(json!({"text": "fine", "@odata.type": "#jc.Feedback"})),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{answer}");
}

/// The broker's view decides: its engine's 403 reaches the caller with the platform's slug, a
/// view that does not take the write answers 405 with its `Allow`, and an Endpoint without
/// guards forwards a keyed create as sent.
#[tokio::test]
async fn the_brokers_view_decides_the_write() {
    let realm = common::Realm::new();
    let (upstream, hops) = broker().await;
    let app = app(&repo("decides"), &upstream, &realm);

    let (status, _, body) = send(
        &app,
        Method::PATCH,
        OPEN,
        "/Refused('x')",
        Some(json!({"a": 1})),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert!(body.get("code").is_some(), "the platform's slug: {body}");

    let (status, headers, _) = send(&app, Method::DELETE, OPEN, "/Locked('x')", None).await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(headers[header::ALLOW], "GET, HEAD");

    let (status, ..) = send(
        &app,
        Method::POST,
        OPEN,
        "/Feedback",
        Some(json!({"id": "urn:ngsi-ld:Feedback:chosen"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert!(
        writes(&hops)
            .iter()
            .any(|hop| hop.contains("urn:ngsi-ld:Feedback:chosen")),
        "an Endpoint that does not mint takes the caller's key"
    );
}

/// What the gateway cannot guard it does not forward: a `$batch` is 501, a body that is not
/// JSON 400; `OPTIONS` names the writes the surface takes.
#[tokio::test]
async fn a_batch_and_an_unreadable_body_stay_at_the_gateway() {
    let realm = common::Realm::new();
    let (upstream, hops) = broker().await;
    let app = app(&repo("batch"), &upstream, &realm);
    let (status, _, body) = send(
        &app,
        Method::POST,
        OPEN,
        "/$batch",
        Some(json!({"requests": []})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_IMPLEMENTED, "{body}");
    let request = Request::builder()
        .method(Method::PATCH)
        .uri(format!("/api/endpoint/{OPEN}/odata/v4/Feedback('x')"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from("{not json"))
        .expect("a request");
    let response = app.clone().oneshot(request).await.expect("answers");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(writes(&hops).is_empty());

    let (status, headers, _) = send(&app, Method::OPTIONS, OPEN, "/Feedback", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(headers[header::ALLOW]
        .to_str()
        .is_ok_and(|allow| allow.contains("POST") && allow.contains("DELETE")));
}

/// T-4929: every 501 and every problem of the gateway on the OData surface is the OData error
/// object (OData Part 1 9.4); a feature the view does not serve is named, not a failure to retry,
/// and the broker's words stay in the log (T-2260).
#[tokio::test]
async fn every_refusal_is_the_odata_error_object() {
    let realm = common::Realm::new();
    let (upstream, _) = broker().await;
    let app = app(&repo("error-object"), &upstream, &realm);
    let cases = [
        (
            Method::GET,
            OPEN,
            "/$crossjoin(Feedback,Plate)",
            StatusCode::NOT_IMPLEMENTED,
            "odata-not-implemented",
        ),
        (
            Method::POST,
            OPEN,
            "/$batch",
            StatusCode::NOT_IMPLEMENTED,
            "odata-batch-unavailable",
        ),
        (
            Method::GET,
            OPEN,
            "/Broken",
            StatusCode::INTERNAL_SERVER_ERROR,
            "broker-failure",
        ),
        (
            Method::GET,
            "zzzzzzzzzzzzzzzzzzzzzzzzzz",
            "/Feedback",
            StatusCode::NOT_FOUND,
            "",
        ),
    ];
    for (method, slug, rest, want, code) in cases {
        let body = (method == Method::POST).then(|| json!({"requests": []}));
        let (status, headers, body) = send(&app, method, slug, rest, body).await;
        assert_eq!(status, want, "{rest}: {body}");
        assert_eq!(headers[header::CONTENT_TYPE], "application/json", "{rest}");
        let error = &body["error"];
        assert!(
            error["code"].is_string() && error["message"].is_string(),
            "{rest}: {body}"
        );
        assert!(code.is_empty() || error["code"] == code, "{rest}: {body}");
        assert!(!body.to_string().contains("hunter2"), "{rest}: {body}");
    }
}

/// S-49 (T-5955): a read-only connector's token forges OData writes on an Endpoint that takes
/// anonymous ones; each is 403 and none reaches the broker.
#[tokio::test]
async fn a_read_only_token_writes_nothing_through_odata() {
    let realm = common::Realm::new();
    let (upstream, hops) = broker().await;
    let app = app(&repo("read-only"), &upstream, &realm);
    let token = realm.mint(&json!({
        "iss": common::ISSUER,
        "sub": "0f5a",
        "aud": "context-gateway",
        "preferred_username": "jana",
        "scope": "openid jc:read-only",
        "exp": common::in_seconds(300),
        "iat": common::in_seconds(-10),
    }));
    for (method, rest, body) in [
        (
            Method::POST,
            "/Feedback",
            Some(json!({"id": "urn:ngsi-ld:Feedback:forged"})),
        ),
        (
            Method::PATCH,
            "/Feedback('x')",
            Some(json!({"text": "forged"})),
        ),
        (Method::DELETE, "/Feedback('x')", None),
    ] {
        let mut request = Request::builder()
            .method(method.clone())
            .uri(format!("/api/endpoint/{OPEN}/odata/v4{rest}"))
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header("jc-reason", "S-49");
        let body = match body {
            Some(body) => {
                request = request.header(header::CONTENT_TYPE, "application/json");
                Body::from(body.to_string())
            }
            None => Body::empty(),
        };
        let response = app
            .clone()
            .oneshot(request.body(body).expect("a request"))
            .await
            .expect("the gateway answers");
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{method} {rest}");
    }
    assert!(writes(&hops).is_empty(), "{:?}", writes(&hops));
}
