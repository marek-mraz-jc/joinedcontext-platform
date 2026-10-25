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
        deprecation: None,
        feeds: Vec::new(),
        calendars: Vec::new(),
        map_defaults: Default::default(),
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
    // ADR-N-057: a prohibition rule wins over every permission (GW4).
    assert_eq!(document["conflict"], "prohibit");
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
    assert!(
        body.contains(" ;\n  odrl:conflict odrl:prohibit ;"),
        "{body}"
    );
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

/// A broker whose engine grants `queryEntity` and nothing else, recording every hop.
async fn deciding_broker() -> (String, Hops) {
    let hops: Hops = Arc::default();
    let recorder = Arc::clone(&hops);
    let app = Router::new().fallback(move |request: Request<Body>| {
        let recorder = Arc::clone(&recorder);
        async move {
            let (parts, sent) = request.into_parts();
            let sent = axum::body::to_bytes(sent, usize::MAX)
                .await
                .expect("the hop's body");
            let asked: Value = serde_json::from_slice(&sent).expect("a JSON check");
            recorder.lock().expect("the hop log").push(Hop {
                method: parts.method,
                path: parts.uri.path().to_owned(),
                headers: parts.headers,
                body: sent.to_vec(),
            });
            let granted = asked["action"]["name"] == "queryEntity";
            axum::Json(json!({ "decision": granted })).into_response()
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

fn evaluate(path: &str, body: &Value) -> Request<Body> {
    Request::builder()
        .method(Method::POST)
        .uri(format!("/api/endpoint/{SLUG}/access/v1/{path}"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .expect("a request")
}

/// EP-165, EP-06 (ADR-N-057 decision 3): an AuthZEN evaluation is the view's check, the same hop and
/// the same answer as `access/check` for allow and deny, sent without the subject the body named.
#[tokio::test]
async fn an_evaluation_is_the_views_check_for_allow_and_deny() {
    for (action, decision) in [("queryEntity", true), ("deleteEntity", false)] {
        let (upstream, hops) = deciding_broker().await;
        let asked = json!({
            "subject": { "type": "role", "id": "public" },
            "action": { "name": action },
            "resource": { "type": "AirQualityObserved" }
        });
        let (status, _, body) = call(
            app(upstream.clone(), endpoint()),
            evaluate("evaluation", &asked),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            serde_json::from_str::<Value>(&body).expect("JSON"),
            json!({ "decision": decision })
        );
        let plain =
            json!({ "action": { "name": action }, "resource": { "type": "AirQualityObserved" } });
        let (status, _, checked) = call(app(upstream, endpoint()), check(&plain.to_string())).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(checked, body, "{action}: evaluation and check agree");

        let hops = hops.lock().expect("the hop log");
        assert_eq!(hops.len(), 2);
        assert_eq!(hops[0].path, format!("/x/views/{SLUG}/access/check"));
        assert_eq!(
            serde_json::from_slice::<Value>(&hops[0].body).expect("JSON"),
            plain,
            "the subject never reaches the engine"
        );
        // The anonymous caller is asked as the public role alone.
        assert_eq!(hops[0].headers["x-jc-roles"], "public");
        assert!(!hops[0].headers.contains_key("x-jc-subject"));
    }
}

/// EP-165: a batch answers each item in the order asked, each item's members over the request's defaults.
#[tokio::test]
async fn a_batch_of_mixed_items_answers_each_in_order() {
    let (upstream, hops) = deciding_broker().await;
    let asked = json!({
        "resource": { "type": "AirQualityObserved" },
        "evaluations": [
            { "action": { "name": "queryEntity" } },
            { "action": { "name": "deleteEntity" } },
            { "action": { "name": "queryEntity" }, "resource": { "type": "Device" } },
            { "action": { "name": "createEntity" } }
        ],
        "options": { "evaluations_semantic": "execute_all" }
    });
    let (status, _, body) = call(app(upstream, endpoint()), evaluate("evaluations", &asked)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        serde_json::from_str::<Value>(&body).expect("JSON"),
        json!({ "evaluations": [
            { "decision": true }, { "decision": false }, { "decision": true }, { "decision": false }
        ] })
    );
    let hops = hops.lock().expect("the hop log");
    let mut types: Vec<Value> = hops
        .iter()
        .map(|hop| {
            serde_json::from_slice::<Value>(&hop.body).expect("JSON")["resource"]["type"].clone()
        })
        .collect();
    types.sort_by_key(Value::to_string);
    assert_eq!(
        types,
        [
            json!("AirQualityObserved"),
            json!("AirQualityObserved"),
            json!("AirQualityObserved"),
            json!("Device")
        ]
    );
}

/// EP-165: a batch outside 1 to 100 items, an unserved semantic or one malformed item is refused whole,
/// and the broker is not asked.
#[tokio::test]
async fn a_batch_the_gateway_cannot_answer_never_reaches_the_broker() {
    let item = json!({ "action": { "name": "queryEntity" } });
    for (asked, detail) in [
        (
            json!({ "evaluations": vec![item.clone(); 101] }),
            "101 items",
        ),
        (json!({ "evaluations": [] }), "0 items"),
        (json!({ "evaluations": [item.clone(), 5] }), "JSON object"),
        (
            json!({ "action": { "name": "queryEntity" } }),
            "evaluations is required",
        ),
        (
            json!({ "evaluations": [item.clone()], "options": { "evaluations_semantic": "deny_on_first_deny" } }),
            "execute_all",
        ),
        (
            json!({ "evaluations": [item.clone(), { "resource": { "type": "T" } }] }),
            "action.name",
        ),
    ] {
        let (upstream, hops) = deciding_broker().await;
        let (status, _, body) =
            call(app(upstream, endpoint()), evaluate("evaluations", &asked)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{detail}: {body}");
        assert!(body.contains(detail), "{detail}: {body}");
        assert!(hops.lock().expect("the hop log").is_empty(), "{detail}");
    }
    let (upstream, _) = deciding_broker().await;
    let (status, _, _) = call(
        app(upstream, endpoint()),
        evaluate("evaluations", &json!({ "evaluations": vec![item; 100] })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "100 items are answered");
}

/// EP-165: the subject is the caller's token: a body naming anyone else, on one evaluation, as a batch's
/// default or on one item, is a 403 and nobody is asked (only `access/simulate` asks for others).
#[tokio::test]
async fn a_foreign_subject_is_forbidden() {
    let jana = json!({ "type": "user", "id": "jana" });
    let item = json!({ "action": { "name": "queryEntity" } });
    for (path, asked) in [
        (
            "evaluation",
            json!({ "subject": jana, "action": { "name": "queryEntity" } }),
        ),
        (
            "evaluations",
            json!({ "subject": jana, "evaluations": [item] }),
        ),
        (
            "evaluations",
            json!({ "evaluations": [item, { "subject": jana, "action": { "name": "queryEntity" } }] }),
        ),
    ] {
        let (upstream, hops) = deciding_broker().await;
        let (status, _, body) = call(app(upstream, endpoint()), evaluate(path, &asked)).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{asked}: {body}");
        assert!(!body.contains("jana"), "{body}");
        assert!(hops.lock().expect("the hop log").is_empty(), "{asked}");
    }
}

/// EP-165, EP-20: a batch pays one request of the caller's rate limit per decision it asks; a batch
/// the bucket cannot pay whole is a 429 that spends nothing and asks nobody.
#[tokio::test]
async fn a_batch_pays_the_rate_limit_per_decision() {
    let (upstream, hops) = deciding_broker().await;
    let mut limited = endpoint();
    limited.rate_limit = Some(RateLimits {
        requests_per_minute: 1,
        burst: Some(10),
    });
    let app = app(upstream, limited);
    let items = |n: usize| json!({ "evaluations": vec![json!({ "action": { "name": "queryEntity" } }); n] });

    let (status, headers, body) = call(app.clone(), evaluate("evaluations", &items(4))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(headers["ratelimit-remaining"], "6");

    let (status, headers, body) = call(app.clone(), evaluate("evaluations", &items(7))).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert!(headers.contains_key("retry-after"));
    assert_eq!(
        hops.lock().expect("the hop log").len(),
        4,
        "the refused batch asked nobody"
    );

    // The refused batch spent only the request itself: five are left for five decisions.
    let (status, _, body) = call(app, evaluate("evaluations", &items(5))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(hops.lock().expect("the hop log").len(), 9);
}

/// EP-165: an evaluation body over 64 KiB, the cap of `access/check`, is a 400; so is a batch whose
/// defaults copied into each item come to more than 1 MiB of broker bodies.
#[tokio::test]
async fn an_evaluation_over_its_size_caps_never_reaches_the_broker() {
    let big = "x".repeat(65 * 1024);
    let over_body = json!({ "action": { "name": "queryEntity" }, "context": { "note": big } });
    let context = "x".repeat(20 * 1024);
    let copied = json!({
        "context": { "note": context },
        "evaluations": vec![json!({ "action": { "name": "queryEntity" } }); 60]
    });
    for (path, asked, detail) in [
        ("evaluation", &over_body, "64 KiB"),
        ("evaluations", &over_body, "64 KiB"),
        ("evaluations", &copied, "1 MiB"),
    ] {
        let (upstream, hops) = deciding_broker().await;
        let (status, _, body) = call(app(upstream, endpoint()), evaluate(path, asked)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{path}: {body}");
        assert!(body.contains(detail), "{path}: {body}");
        assert!(hops.lock().expect("the hop log").is_empty(), "{path}");
    }
}

/// An Endpoint of the model `air` with one class, `AirQualityObserved`, of `count` attributes
/// `a0`, `a1`, … beside `pm10` and the hidden `stationOwner`.
fn modelled(count: usize) -> Endpoint {
    use context_gateway::resolver::DeclaredTypes;
    use jc_manifest::model_schema::ModelSchema;
    let mut properties = serde_json::Map::new();
    for name in ["id", "pm10", "stationOwner"]
        .into_iter()
        .map(str::to_owned)
        .chain((0..count).map(|i| format!("a{i}")))
    {
        properties.insert(name, json!({ "type": "string" }));
    }
    let model = json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "definitions": { "AirQualityObserved": { "type": "object", "properties": properties } }
    });
    let mut modelled = endpoint();
    modelled.hidden_attributes = ["stationOwner".to_owned()].into();
    modelled.declared_types = Some(Arc::new(DeclaredTypes {
        model: "air".into(),
        classes: ["AirQualityObserved".to_owned()].into(),
        units: Default::default(),
        relationships: Default::default(),
        quality: Vec::new(),
        schema: Some(Arc::new(ModelSchema::compile(
            "air",
            "1.0.0",
            &model,
            &["AirQualityObserved".to_owned()],
            true,
        ))),
    }));
    modelled
}

fn attributes(query: &str) -> Request<Body> {
    Request::builder()
        .uri(format!("/api/endpoint/{SLUG}/access/attributes{query}"))
        .body(Body::empty())
        .expect("a request")
}

/// T-5527: `access/attributes` asks the view once with the served slots of the type (a hidden
/// one left out) under the pinned caller, a forged one stripped, and passes the table on.
#[tokio::test]
async fn the_attribute_table_is_one_ask_of_the_view_with_the_served_slots() {
    let table = json!({"type": "AirQualityObserved", "attributes": [
        {"name": "pm10", "read": true, "write": false, "delete": false, "reason": "grant"}]});
    let (upstream, hops) = json_broker(StatusCode::OK, table.clone()).await;
    let mut request = attributes("?type=AirQualityObserved");
    request
        .headers_mut()
        .insert("x-jc-roles", "admin".parse().expect("a header value"));
    let (status, _, body) = call(app(upstream, modelled(1)), request).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(serde_json::from_str::<Value>(&body).expect("JSON"), table);
    let hop = one_hop(&hops);
    assert_eq!(hop.method, Method::POST);
    assert_eq!(hop.path, format!("/x/views/{SLUG}/access/attributes"));
    assert_eq!(hop.headers["x-jc-roles"], "public");
    assert_eq!(hop.headers["ngsild-tenant"], SPACE);
    let asked: Value = serde_json::from_slice(&hop.body).expect("a JSON ask");
    assert_eq!(
        asked,
        json!({"type": "AirQualityObserved", "attributes": ["a0", "pm10"]})
    );
}

/// T-5527: a type the model lacks, no type, an Endpoint without a model and a class of more than
/// 200 served attributes are a 400 naming the field, and nobody is asked; a members-only Endpoint
/// answers an anonymous caller 401 first.
#[tokio::test]
async fn an_attribute_table_the_gateway_cannot_answer_never_reaches_the_broker() {
    let (upstream, hops) = json_broker(StatusCode::OK, json!({})).await;
    for (endpoint, query, detail) in [
        (modelled(1), "", "type is required"),
        (modelled(1), "?type=", "type is required"),
        (modelled(1), "?type=Nosuch", "not a class"),
        (endpoint(), "?type=AirQualityObserved", "not a class"),
        (
            modelled(200),
            "?type=AirQualityObserved",
            "201 served attributes",
        ),
    ] {
        let (status, _, body) = call(app(upstream.clone(), endpoint), attributes(query)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{query}: {body}");
        assert!(body.contains(detail), "{query}: {body}");
    }
    let mut members = modelled(1);
    members.audience = Audience::Organization;
    let (status, _, _) = call(
        app(upstream, members),
        attributes("?type=AirQualityObserved"),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(hops.lock().expect("the hop log").is_empty());
}

/// T-5527: the table pays the rate limit one request per 100 decisions, three per attribute: 101
/// attributes are 303 decisions, four requests.
#[tokio::test]
async fn the_attribute_table_pays_the_rate_limit_per_hundred_decisions() {
    let (upstream, hops) = json_broker(StatusCode::OK, json!({"attributes": []})).await;
    let mut limited = modelled(100);
    limited.rate_limit = Some(RateLimits {
        requests_per_minute: 1,
        burst: Some(6),
    });
    let app = app(upstream, limited);
    let (status, headers, body) = call(app.clone(), attributes("?type=AirQualityObserved")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(headers["ratelimit-remaining"], "2");
    let (status, _, _) = call(app, attributes("?type=AirQualityObserved")).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(hops.lock().expect("the hop log").len(), 1);
}
