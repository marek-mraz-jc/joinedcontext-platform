//! The access document and the AuthZEN check of an Endpoint are the broker's view's `access`
//! surface (T-3649, plan 5.6, map 7.5): `/api/endpoint/{slug}/access` reads
//! `/x/views/{slug}/access` and `.../access/check` posts to `/x/views/{slug}/access/check`, under
//! the tenant and the verified caller. The broker's engine says what the caller holds; these
//! tests hold what the gateway adds around that hop (the Endpoint's name, space, rate, licence
//! duties and public `uid`), what it never lets through, and which Endpoints keep the gateway's
//! own document because no view serves them yet.

use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode};
use axum::response::IntoResponse;
use axum::Router;
use context_gateway::app::{router, Gateway};
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::Endpoint;
use context_gateway::view_mapping::ViewMapping;
use jc_core::kinds::{Audience, Catalog, PolicySpec, RateLimits, Representation};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

const SLUG: &str = "k4y7pq2mzt6vhx3nbwrs5cjd8f";
const SPACE: &str = "ovzdusie";
const DOMAIN: &str = "banskabystrica.sk";
const BASE: &str = "https://dev.joinedcontext.com";
const ODRL: &str = "application/odrl+json";
const GRANT_AST: &str = "application/vnd.joinedcontext.grant-ast+json";

/// One hop as the broker received it.
#[derive(Clone)]
struct Hop {
    method: Method,
    path: String,
    headers: HeaderMap,
    body: Vec<u8>,
}

type Hops = Arc<Mutex<Vec<Hop>>>;

/// A broker that records every hop and answers each with `status`, `content_type` and `body`.
async fn broker(status: StatusCode, content_type: &'static str, body: String) -> (String, Hops) {
    let hops: Hops = Arc::default();
    let recorder = Arc::clone(&hops);
    let app = Router::new().fallback(move |request: Request<Body>| {
        let (recorder, body) = (Arc::clone(&recorder), body.clone());
        async move {
            let (parts, sent) = request.into_parts();
            let sent = axum::body::to_bytes(sent, usize::MAX)
                .await
                .expect("the hop's body");
            recorder.lock().expect("the hop log").push(Hop {
                method: parts.method,
                path: parts.uri.path().to_owned(),
                headers: parts.headers,
                body: sent.to_vec(),
            });
            (status, [("content-type", content_type)], body).into_response()
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

async fn json_broker(status: StatusCode, body: Value) -> (String, Hops) {
    broker(status, "application/json", body.to_string()).await
}

fn grant() -> PolicySpec {
    serde_norway::from_str(&format!(
        "contextSpaceRef: {SPACE}\nassigner: did:web:{DOMAIN}\nassignee: {{ kind: role, id: public }}\n\
         operations: [queryEntity, retrieveEntity]\n\
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
        representations: vec![Representation::NgsiLd],
        rate_limit: None,
        creates: None,
        file_limits: None,
        hidden_attributes: Default::default(),
        projection: None,
        models: Vec::new(),
        view_mapping: None,
        catalog: None,
        policy_names: Vec::new(),
        manifest_name: String::new(),
        policies: vec![grant()],
    }
}

fn app(upstream: String, endpoint: Endpoint) -> Router {
    let mut gateway = Gateway::new(Broker::new(upstream), Box::new(PolicyPdp), DOMAIN);
    gateway.public_url = Some(BASE.to_owned());
    router(Arc::new(gateway.serve([endpoint])))
}

async fn call(app: Router, request: Request<Body>) -> (StatusCode, HeaderMap, String) {
    let response = app.oneshot(request).await.expect("the gateway answers");
    let (status, headers) = (response.status(), response.headers().clone());
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("a body");
    (
        status,
        headers,
        String::from_utf8_lossy(&bytes).into_owned(),
    )
}

fn get(accept: &str) -> Request<Body> {
    Request::builder()
        .uri(format!("/api/endpoint/{SLUG}/access"))
        .header("accept", accept)
        .header("accept-encoding", "gzip")
        .header("cookie", "session=not-for-the-broker")
        .body(Body::empty())
        .expect("a request")
}

fn check(body: &str) -> Request<Body> {
    Request::builder()
        .method(Method::POST)
        .uri(format!("/api/endpoint/{SLUG}/access/check"))
        .header("content-type", "application/json")
        .header("cookie", "session=not-for-the-broker")
        .body(Body::from(body.to_owned()))
        .expect("a request")
}

/// The broker's AuthZEN document for the anonymous caller (T-3638, addon README section 4).
fn view_document() -> Value {
    json!({
        "subject": { "type": "role", "id": "public" },
        "resource": { "type": "view", "id": SLUG },
        "permissions": [{
            "resource": { "type": "AirQualityObserved" },
            "actions": ["queryEntity", "retrieveEntity"],
            "attributes": "*",
            "constraints": {}
        }],
        "prohibitions": []
    })
}

/// The broker's ODRL `Set`, its `uid` on the broker's own address.
fn view_odrl() -> Value {
    json!({
        "@context": ["http://www.w3.org/ns/odrl.jsonld",
                     "https://joinedcontext.com/odrl/ngsi-ld/v1/context.jsonld"],
        "@type": "Set",
        "uid": format!("http://antares:9090/x/views/{SLUG}/access#sha256:00"),
        "assigner": format!("did:web:{DOMAIN}"),
        "assignee": "public",
        "permission": [{
            "action": "queryEntity",
            "target": { "@type": "ngsi-ld:EntityType", "uid": "AirQualityObserved" }
        }]
    })
}

fn one_hop(hops: &Hops) -> Hop {
    let hops = hops.lock().expect("the hop log");
    assert_eq!(hops.len(), 1, "one hop to the broker");
    hops[0].clone()
}

/// Plan 5.6: the document is the view's, asked under the tenant and the verified caller, with
/// none of the caller's own headers: not their cookie, not an encoding the gateway would have to
/// undo before it adds the Endpoint's members.
#[tokio::test]
async fn the_document_is_read_from_the_view_under_the_pinned_caller() {
    let (upstream, hops) = json_broker(StatusCode::OK, view_document()).await;
    let (status, _, body) = call(app(upstream, endpoint()), get("application/json")).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let hop = one_hop(&hops);
    assert_eq!(hop.method, Method::GET);
    assert_eq!(hop.path, format!("/x/views/{SLUG}/access"));
    assert_eq!(hop.headers["ngsild-tenant"], SPACE);
    assert_eq!(hop.headers["x-jc-roles"], "public");
    assert_eq!(hop.headers["accept"], "application/json");
    for absent in ["cookie", "authorization", "accept-encoding"] {
        assert!(
            !hop.headers.contains_key(absent),
            "{absent} reached the broker"
        );
    }

    let document: Value = serde_json::from_str(&body).expect("JSON");
    assert_eq!(document["permissions"], view_document()["permissions"]);
    assert_eq!(
        document["subject"],
        json!({ "type": "role", "id": "public" })
    );
}

/// EP-55, EP-56: the caller was admitted to this Endpoint, so the document names it and its
/// space as the Portal's matrix shows them, and the rate it holds every caller to. The view's
/// own id never reaches the caller as the resource.
#[tokio::test]
async fn the_document_names_the_endpoint_its_space_and_its_rate() {
    let mut limited = endpoint();
    limited.rate_limit = Some(RateLimits {
        requests_per_minute: 600,
        burst: Some(50),
    });
    let (upstream, _) = json_broker(StatusCode::OK, view_document()).await;
    let (_, _, body) = call(app(upstream, limited), get("application/json")).await;
    let document: Value = serde_json::from_str(&body).expect("JSON");
    assert_eq!(
        document["resource"],
        json!({ "type": "endpoint", "id": SLUG, "space": SPACE })
    );
    assert_eq!(
        document["limits"],
        json!({ "requestsPerMinute": 600, "burst": 50 })
    );

    let (upstream, _) = json_broker(StatusCode::OK, view_document()).await;
    let (_, _, body) = call(app(upstream, endpoint()), get("application/json")).await;
    let document: Value = serde_json::from_str(&body).expect("JSON");
    assert!(document.get("limits").is_none(), "{document:#}");
}

/// EP-57, EP-79: ODRL is read from the view as JSON-LD; the gateway puts the licence's duties on
/// every permission and names its own public access URL in the `uid`, never the broker's.
#[tokio::test]
async fn odrl_carries_the_licence_duties_and_the_public_uid() {
    let mut licensed = endpoint();
    let catalog: Catalog = serde_norway::from_str("license: CC_BY_4_0\n").expect("a catalog");
    licensed.catalog = Some(Arc::new(catalog));
    let (upstream, hops) = json_broker(StatusCode::OK, view_odrl()).await;
    let (status, headers, body) = call(app(upstream, licensed), get(ODRL)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(headers["content-type"], ODRL);
    assert_eq!(one_hop(&hops).headers["accept"], ODRL);

    let document: Value = serde_json::from_str(&body).expect("JSON");
    let uid = document["uid"].as_str().expect("a uid");
    assert!(
        uid.starts_with(&format!("{BASE}/api/endpoint/{SLUG}/access#sha256:")),
        "{uid}"
    );
    assert!(!body.contains("/x/views/"), "{body}");
    assert_eq!(
        document["permission"][0]["duty"],
        json!([{ "action": "attribute" }])
    );
    assert_eq!(document["assigner"], format!("did:web:{DOMAIN}"));
}

/// EP-59: the `uid` is the digest of the rules as published, so a licence that adds a duty is a
/// new identifier for the same grants.
#[tokio::test]
async fn a_licence_changes_the_uid_of_the_same_grants() {
    let (upstream, _) = json_broker(StatusCode::OK, view_odrl()).await;
    let (_, _, plain) = call(app(upstream, endpoint()), get(ODRL)).await;
    let mut licensed = endpoint();
    let catalog: Catalog = serde_norway::from_str("license: CC_BY_4_0\n").expect("a catalog");
    licensed.catalog = Some(Arc::new(catalog));
    let (upstream, _) = json_broker(StatusCode::OK, view_odrl()).await;
    let (_, _, with_duty) = call(app(upstream, licensed), get(ODRL)).await;
    let uid = |body: &str| serde_json::from_str::<Value>(body).expect("JSON")["uid"].clone();
    assert_ne!(uid(&plain), uid(&with_duty));
    assert!(
        serde_json::from_str::<Value>(&plain).expect("JSON")["permission"][0]
            .get("duty")
            .is_none()
    );
}

/// EP-57: Turtle is written by the gateway from the published ODRL document, so both say the
/// same `uid`; the broker is asked for JSON-LD.
#[tokio::test]
async fn turtle_is_written_from_the_published_odrl() {
    let (upstream, hops) = json_broker(StatusCode::OK, view_odrl()).await;
    let (status, headers, body) = call(app(upstream, endpoint()), get("text/turtle")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(headers["content-type"], "text/turtle");
    assert_eq!(one_hop(&hops).headers["accept"], ODRL);
    assert!(
        body.contains(&format!("<{BASE}/api/endpoint/{SLUG}/access#sha256:")),
        "{body}"
    );
    assert!(!body.contains("/x/views/"), "{body}");
}

/// EP-58: the grant tree goes through as the broker wrote it, under its own media type.
#[tokio::test]
async fn the_grant_tree_is_the_views() {
    let tree = json!({ "AirQualityObserved": { "actions": ["queryEntity"] } });
    let (upstream, hops) = broker(StatusCode::OK, GRANT_AST, tree.to_string()).await;
    let (status, headers, body) = call(app(upstream, endpoint()), get(GRANT_AST)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(headers["content-type"], GRANT_AST);
    assert_eq!(one_hop(&hops).headers["accept"], GRANT_AST);
    assert_eq!(serde_json::from_str::<Value>(&body).expect("JSON"), tree);
}

/// R51: the check is the engine's decision on the request as the caller wrote it.
#[tokio::test]
async fn the_check_is_the_views_decision_on_the_request_as_sent() {
    let decision = json!({ "decision": true, "context": { "reason": "policy_grant_matched" } });
    let (upstream, hops) = json_broker(StatusCode::OK, decision.clone()).await;
    let asked = r#"{"action":{"name":"queryEntity"},"resource":{"type":"AirQualityObserved"}}"#;
    let (status, _, body) = call(app(upstream, endpoint()), check(asked)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        serde_json::from_str::<Value>(&body).expect("JSON"),
        decision
    );

    let hop = one_hop(&hops);
    assert_eq!(hop.method, Method::POST);
    assert_eq!(hop.path, format!("/x/views/{SLUG}/access/check"));
    assert_eq!(hop.body, asked.as_bytes());
    assert_eq!(hop.headers["content-type"], "application/json");
    assert_eq!(hop.headers["ngsild-tenant"], SPACE);
    assert!(!hop.headers.contains_key("cookie"));
}

/// A request the check cannot read is refused by the gateway, whoever decides, and the broker
/// is not asked.
#[tokio::test]
async fn a_malformed_check_never_reaches_the_broker() {
    for malformed in [
        "not json",
        r#"{"resource":{"type":"T"}}"#,
        r#"{"action":{}}"#,
    ] {
        let (upstream, hops) = json_broker(StatusCode::OK, json!({ "decision": true })).await;
        let (status, _, body) = call(app(upstream, endpoint()), check(malformed)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{malformed}: {body}");
        assert!(!body.contains(malformed), "{body}");
        assert!(hops.lock().expect("the hop log").is_empty(), "{malformed}");
    }
    let (upstream, hops) = json_broker(StatusCode::OK, json!({ "decision": true })).await;
    let oversized = format!(r#"{{"action":{{"name":"{}"}}}}"#, "a".repeat(70 * 1024));
    let (status, _, _) = call(app(upstream, endpoint()), check(&oversized)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(hops.lock().expect("the hop log").is_empty());
}

/// A broker failure is the gateway's own `5xx` document: what the broker held never reaches the
/// caller (T-2260), on the document and on the check alike.
#[tokio::test]
async fn a_broker_failure_names_nothing_the_broker_held() {
    let leak = json!({ "detail": "tenant ovzdusie: rule table of did:web:other.sk" });
    let (upstream, _) = json_broker(StatusCode::INTERNAL_SERVER_ERROR, leak.clone()).await;
    let (status, _, body) = call(app(upstream, endpoint()), get("application/json")).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(!body.contains("other.sk"), "{body}");

    let (upstream, _) = json_broker(StatusCode::BAD_GATEWAY, leak).await;
    let (status, _, body) = call(
        app(upstream, endpoint()),
        check(r#"{"action":{"name":"queryEntity"}}"#),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(!body.contains("other.sk"), "{body}");
}

/// A success the gateway cannot read is a `502`, never an empty or invented document.
#[tokio::test]
async fn an_unreadable_answer_is_a_bad_gateway() {
    let (upstream, _) = broker(StatusCode::OK, "text/plain", "rules: yes".to_owned()).await;
    let (status, _, body) = call(app(upstream, endpoint()), get("application/json")).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
    assert!(!body.contains("rules"), "{body}");

    // Every document and decision is a JSON object; an array is passed on in no representation.
    for accept in ["application/json", ODRL, "text/turtle", GRANT_AST] {
        let (upstream, _) = json_broker(StatusCode::OK, json!(["rule-of-another"])).await;
        let (status, _, body) = call(app(upstream, endpoint()), get(accept)).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY, "{accept}: {body}");
        assert!(!body.contains("rule-of-another"), "{accept}: {body}");
    }
    let (upstream, _) = json_broker(StatusCode::OK, json!([true])).await;
    let (status, _, _) = call(
        app(upstream, endpoint()),
        check(r#"{"action":{"name":"queryEntity"}}"#),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
}

/// EP-21, K36: what a client writes in the headers the broker decides on never reaches it: the
/// tenant is the Endpoint's space and the caller the one the gateway verified, on the document
/// and on the check.
#[tokio::test]
async fn a_forged_caller_never_reaches_the_view() {
    let forge = |mut request: Request<Body>| {
        for (name, value) in [
            ("ngsild-tenant", "somebody-elses-space"),
            ("x-jc-subject", "spravca"),
            ("x-jc-client", "portal-api"),
            ("x-jc-groups", "data-steward"),
            ("x-jc-roles", "admin"),
        ] {
            request
                .headers_mut()
                .insert(name, value.parse().expect("a header value"));
        }
        request
    };
    let (upstream, hops) = json_broker(StatusCode::OK, view_document()).await;
    call(
        app(upstream.clone(), endpoint()),
        forge(get("application/json")),
    )
    .await;
    call(
        app(upstream, endpoint()),
        forge(check(r#"{"action":{"name":"queryEntity"}}"#)),
    )
    .await;
    let hops = hops.lock().expect("the hop log");
    assert_eq!(hops.len(), 2);
    for hop in hops.iter() {
        assert_eq!(hop.headers["ngsild-tenant"], SPACE, "{}", hop.path);
        assert_eq!(hop.headers["x-jc-roles"], "public", "{}", hop.path);
        for absent in ["x-jc-subject", "x-jc-client", "x-jc-groups"] {
            assert!(
                !hop.headers.contains_key(absent),
                "{absent} on {}",
                hop.path
            );
        }
    }
}

/// EP-56 to EP-58: the offer the caller wrote first decides the representation, the view is
/// asked for the JSON it is built from, and the answer is labelled as what it is. Anything this
/// surface does not serve is the AuthZEN document, never a 406.
#[tokio::test]
async fn each_offer_asks_the_view_for_its_json_and_is_labelled_as_itself() {
    for (accept, asked, labelled) in [
        ("application/json", "application/json", "application/json"),
        (
            "application/ld+json",
            "application/json",
            "application/json",
        ),
        ("*/*", "application/json", "application/json"),
        ("", "application/json", "application/json"),
        (ODRL, ODRL, ODRL),
        (
            "text/turtle;charset=utf-8, application/json",
            ODRL,
            "text/turtle",
        ),
        (
            "application/json;q=0.1, text/turtle;q=1",
            "application/json",
            "application/json",
        ),
        (GRANT_AST, GRANT_AST, GRANT_AST),
    ] {
        let (upstream, hops) = json_broker(StatusCode::OK, view_odrl()).await;
        let (status, headers, body) = call(app(upstream, endpoint()), get(accept)).await;
        assert_eq!(status, StatusCode::OK, "{accept:?}: {body}");
        assert_eq!(one_hop(&hops).headers["accept"], asked, "{accept:?}");
        assert_eq!(headers["content-type"], labelled, "{accept:?}");
    }
}

/// A space's canonical surface is the broker's `cs-{space}` view (plan 5.1): its MCP
/// `describe_access` reads that view's access surface, never one named after the space alone.
#[tokio::test]
async fn the_space_reads_the_access_of_its_own_view() {
    use context_gateway::resolver::Space;
    let (upstream, hops) = json_broker(StatusCode::OK, view_document()).await;
    let space = Space {
        endpoint: Arc::new(Endpoint {
            slug: SPACE.to_owned(),
            base_path: format!("/cs/{SPACE}"),
            representations: vec![Representation::NgsiLd, Representation::Mcp],
            ..endpoint()
        }),
        title: Default::default(),
        description: Default::default(),
        is_sandbox: false,
        default_locale: None,
    };
    let mut gateway = Gateway::new(Broker::new(upstream), Box::new(PolicyPdp), DOMAIN);
    gateway.public_url = Some(BASE.to_owned());
    let app = router(Arc::new(gateway.serve_spaces([space])));
    let request = Request::builder()
        .method(Method::POST)
        .uri(format!("/cs/{SPACE}/mcp"))
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(Body::from(
            json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                    "params": { "name": "describe_access", "arguments": {} } })
            .to_string(),
        ))
        .expect("a request");
    let (status, _, body) = call(app, request).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(!body.contains("\"isError\":true"), "{body}");
    assert_eq!(one_hop(&hops).path, format!("/x/views/cs-{SPACE}/access"));
}

/// The broker's own refusal (an unknown view is its `404`) goes to the caller as it came.
#[tokio::test]
async fn the_views_refusal_is_passed_on() {
    let missing = json!({ "type": "urn:antares:error:NotFound", "title": "no such view" });
    let (upstream, _) = json_broker(StatusCode::NOT_FOUND, missing.clone()).await;
    let (status, _, body) = call(app(upstream, endpoint()), get("application/json")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(serde_json::from_str::<Value>(&body).expect("JSON"), missing);
}

/// Admission stays the gateway's: an unknown slug and a caller the audience refuses are answered
/// before the broker is asked.
#[tokio::test]
async fn admission_comes_before_the_hop() {
    let (upstream, hops) = json_broker(StatusCode::OK, view_document()).await;
    let request = Request::builder()
        .uri("/api/endpoint/no-such-endpoint/access")
        .body(Body::empty())
        .expect("a request");
    let (status, _, _) = call(app(upstream.clone(), endpoint()), request).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let mut members = endpoint();
    members.audience = Audience::Organization;
    let (status, _, _) = call(app(upstream, members), get("application/json")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(hops.lock().expect("the hop log").is_empty());
}

/// An Endpoint with a view of another model (G18) is served by its broker view like any other
/// (T-3799): the gateway keeps no document or decision of its own (T-3650).
#[tokio::test]
async fn an_endpoint_with_a_view_mapping_is_answered_by_its_view() {
    let mut mapped = endpoint();
    mapped.view_mapping = Some(Arc::new(
        ViewMapping::parse(&json!({
            "version": 2,
            "sourceClass": "Senzor",
            "targetClass": "AirQualityObserved",
            "slots": [{ "target": "pm10", "source": "pm10", "kind": "rename", "filterable": true }]
        }))
        .expect("the IR parses"),
    ));
    let (upstream, hops) = json_broker(StatusCode::OK, json!({ "decision": false })).await;
    let (status, _, body) = call(
        app(upstream, mapped),
        check(r#"{"action":{"name":"queryEntity"},"resource":{"type":"AirQualityObserved"}}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        serde_json::from_str::<Value>(&body).expect("JSON"),
        json!({ "decision": false }),
        "the engine's answer, though the gateway's own Policy grants it"
    );
    let hops = hops.lock().expect("the hop log");
    assert_eq!(hops.len(), 1);
    assert_eq!(hops[0].path, format!("/x/views/{SLUG}/access/check"));
}

/// An Endpoint the broker can hold no view of (a slug longer than a view id, which the Portal's
/// writer refuses) has nobody to decide for it: its document and check are a `503` naming why,
/// never the gateway's own answer, and the broker is not asked (T-3650, fail closed).
#[tokio::test]
async fn an_endpoint_without_a_view_is_answered_by_nobody() {
    let long = "k".repeat(64);
    let mut unviewable = endpoint();
    unviewable.slug = long.clone();
    unviewable.base_path = format!("/api/endpoint/{long}");
    let (upstream, hops) = json_broker(StatusCode::OK, json!({ "decision": true })).await;
    for request in [
        Request::builder()
            .uri(format!("/api/endpoint/{long}/access"))
            .body(Body::empty())
            .expect("a request"),
        Request::builder()
            .method("POST")
            .uri(format!("/api/endpoint/{long}/access/check"))
            .header("content-type", "application/json")
            .body(Body::from(r#"{"action":{"name":"queryEntity"}}"#))
            .expect("a request"),
    ] {
        let (status, headers, body) =
            call(app(upstream.clone(), unviewable.clone()), request).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
        assert_eq!(headers["content-type"], "application/problem+json");
        assert!(
            !body.contains("decision") && !body.contains("permissions"),
            "{body}"
        );
    }
    assert!(hops.lock().expect("the hop log").is_empty());
}

/// T-5045: an agent reading `describe_access` on an Endpoint that serves the WebSocket learns
/// its public address, subprotocol and subscribe message; on one that does not, nothing.
#[tokio::test]
async fn describe_access_names_the_websocket_only_where_it_is_served() {
    let ask = || {
        Request::builder()
            .method(Method::POST)
            .uri(format!("/api/endpoint/{SLUG}/mcp"))
            .header("content-type", "application/json")
            .body(Body::from(
                json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                        "params": { "name": "describe_access", "arguments": {} } })
                .to_string(),
            ))
            .expect("a request")
    };
    let live = |representations: Vec<Representation>| async move {
        let (upstream, _) = json_broker(StatusCode::OK, view_document()).await;
        let served = Endpoint {
            representations,
            ..endpoint()
        };
        let (status, _, body) = call(app(upstream, served), ask()).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let answer: Value = serde_json::from_str(&body).expect("JSON");
        answer["result"]["structuredContent"]["access"]["liveUpdates"].clone()
    };

    let socket = live(vec![Representation::Mcp, Representation::WebSocket]).await;
    assert_eq!(
        socket["address"],
        format!("wss://dev.joinedcontext.com/api/endpoint/{SLUG}/ws")
    );
    assert_eq!(socket["protocol"], "ngsi-ld-ws");
    assert_eq!(socket["subscribe"]["metadata"]["x-op"], "x-subscribe");
    assert_eq!(socket["limits"]["lifetime"]["anonymousSeconds"], 86_400);

    let none = live(vec![Representation::Mcp]).await;
    assert!(none.is_null(), "{none}");
}
