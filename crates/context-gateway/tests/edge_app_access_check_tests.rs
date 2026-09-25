//! Edge cases of `app::access_check` (T-1880, EP-26, MP-02).
//!
//! Contract, in one sentence: `POST /api/endpoint/{slug}/access/check` answers one
//! prospective request yes or no, from the same PDP that would enforce it — the caller is
//! admitted first, then at most 64 KiB of body is read, and the body has to name
//! `action.name` or it is a bad request (T-0163, R51, EP-60).
//!
//! The inputs are the slug, the `Authorization` header and the body's `action.name` and
//! `resource.type`. Three things matter here. The order: nothing of a stranger's body is read
//! before the endpoint has admitted them, so an unknown slug cannot be used to make this pod
//! parse 64 KiB. The answer: a yes may name the assigner whose grant matched, because the
//! caller holds that grant; a no names nothing, because the reason is the rule (GW6). And the
//! decision itself, which must be the same one the data path would make — an operation the
//! caller may not perform is `false` here and refused there.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use context_gateway::app::{router, Gateway};
use context_gateway::auth::accounts::ServiceAccounts;
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::Endpoint;
use jc_core::kinds::{Audience, PolicySpec, Representation};
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

/// A public endpoint: the anonymous caller may read one type and write nothing.
const OPEN: &str = "k4y7pq2mzt6vhx3nbwrs5cjd8f";
/// An endpoint that needs a token.
const CLOSED: &str = "p9d2wc5kzn8mth4rqvb7xj3sfy";

fn policy(yaml: &str) -> PolicySpec {
    serde_norway::from_str(yaml).expect("the policy spec parses")
}

fn grants() -> Vec<PolicySpec> {
    vec![
        policy(
            r#"contextSpaceRef: ovzdusie
assigner: did:web:banskabystrica.sk
assignee: { kind: role, id: public }
operations: [queryEntity, retrieveEntity]
information:
  - entities:
      - type: AirQualityObserved
    propertyNames: [pm10]
"#,
        ),
        policy(
            r#"contextSpaceRef: ovzdusie
assigner: did:web:tajny-uradnik.sk
assignee: { kind: role, id: data-steward }
operations: [createEntity, deleteEntity]
information:
  - entities:
      - type: InternalIncident
"#,
        ),
    ]
}

fn endpoint(slug: &str, audience: Audience) -> Endpoint {
    Endpoint {
        declared_types: None,
        roles: Default::default(),
        slug: slug.to_owned(),
        title: Default::default(),
        description: Default::default(),
        space: "ovzdusie".to_owned(),
        project: "ovzdusie".to_owned(),
        audience,
        allowed_projects: Vec::new(),
        representations: vec![Representation::NgsiLd],
        rate_limit: None,
        file_limits: None,
        hidden_attributes: Default::default(),
        projection: None,
        base_path: format!("/api/endpoint/{slug}"),
        models: Vec::new(),
        view_mapping: None,
        policies: grants(),
    }
}

/// The deployment, trusting one realm: a caller's token has to be minted by the same `Realm`
/// the router was built with, so the fixture takes one rather than making its own.
fn app_of(realm: &common::Realm) -> axum::Router {
    router(Arc::new(
        Gateway::new(
            Broker::new("http://127.0.0.1:1"),
            Box::new(PolicyPdp),
            "banskabystrica.sk",
        )
        .serve([
            endpoint(OPEN, Audience::Public),
            endpoint(CLOSED, Audience::Organization),
        ])
        .authenticate(
            Arc::new(realm.verifier()),
            ServiceAccounts::new(),
            Some("https://bb.example.sk".to_owned()),
        ),
    ))
}

fn app() -> axum::Router {
    app_of(&common::Realm::new())
}

async fn call_on(app: axum::Router, request: Request<Body>) -> (StatusCode, String) {
    let response = app.oneshot(request).await.expect("the gateway answers");
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 256 * 1024)
        .await
        .expect("a readable body");
    (status, String::from_utf8_lossy(&body).into_owned())
}

async fn call(request: Request<Body>) -> (StatusCode, String) {
    call_on(app(), request).await
}

fn check(slug: &str, body: impl Into<Body>) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(format!("/api/endpoint/{slug}/access/check"))
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .body(body.into())
        .expect("a request")
}

fn asking(action: &str, entity_type: Option<&str>) -> Body {
    let mut query = json!({ "action": { "name": action } });
    if let Some(entity_type) = entity_type {
        query["resource"] = json!({ "type": entity_type });
    }
    Body::from(serde_json::to_vec(&query).expect("a query"))
}

/// R51, GW6: the first case, and the one most likely to be red. The check has to agree with
/// the enforcement — a grant the caller does not hold is `false`, and the refusal names
/// nothing of the grant that would have allowed it to somebody else.
#[tokio::test]
async fn an_operation_the_caller_does_not_hold_is_a_bare_no() {
    for (action, entity_type) in [
        ("createEntity", Some("InternalIncident")),
        ("deleteEntity", Some("InternalIncident")),
        ("createEntity", None),
        ("queryEntity", Some("InternalIncident")),
        ("retrieveEntity", Some("InternalIncident")),
    ] {
        let (status, body) = call(check(OPEN, asking(action, entity_type))).await;
        assert_eq!(status, StatusCode::OK, "{action} {entity_type:?}: {body}");

        let answer: Value = serde_json::from_str(&body).expect("a decision");
        assert_eq!(answer["decision"], json!(false), "{action} {entity_type:?}");
        assert_eq!(
            answer.as_object().expect("an object").len(),
            1,
            "a refusal is the decision and nothing else: {body}",
        );
        for secret in ["tajny-uradnik.sk", "data-steward", "InternalIncident"] {
            assert!(!body.contains(secret), "{secret:?} leaked into {body}");
        }
    }
}

/// R51: what the caller does hold is a yes, and it may name the assigner — the caller holds
/// that grant, so the provenance of their own permission is theirs to read.
#[tokio::test]
async fn an_operation_the_caller_holds_is_a_yes_with_its_assigner() {
    let (status, body) = call(check(
        OPEN,
        asking("queryEntity", Some("AirQualityObserved")),
    ))
    .await;

    assert_eq!(status, StatusCode::OK);
    let answer: Value = serde_json::from_str(&body).expect("a decision");
    assert_eq!(answer["decision"], json!(true));
    assert_eq!(
        answer["context"]["assigner"],
        json!("did:web:banskabystrica.sk")
    );
}

/// T-0163: `action.name` is required, and every way of leaving it out is the same bad
/// request. None of them is answered with a decision, because a check that defaulted its
/// action would be answering a question nobody asked.
#[tokio::test]
async fn a_body_without_an_action_name_is_a_bad_request() {
    for body in [
        "{}",
        r#"{"action": {}}"#,
        r#"{"action": null}"#,
        r#"{"action": "queryEntity"}"#,
        r#"{"action": {"name": null}}"#,
        r#"{"action": {"name": 42}}"#,
        r#"{"action": {"name": ["queryEntity"]}}"#,
        r#"{"action": {"Name": "queryEntity"}}"#,
        r#"{"resource": {"type": "AirQualityObserved"}}"#,
        "[]",
        "null",
        "0",
    ] {
        let (status, answer) = call(check(OPEN, body.to_owned())).await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "{body} was answered with {answer}",
        );
        assert!(!answer.contains("decision"), "{body} decided: {answer}");
    }
}

/// A body that is not JSON is a bad request that says so, so a client can tell "I sent
/// nonsense" from "I asked about something you will not allow".
#[tokio::test]
async fn a_body_that_is_not_json_says_so() {
    for body in ["", " ", "not json", "{", "<html/>", "\u{0}"] {
        let (status, answer) = call(check(OPEN, body.to_owned())).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body:?}");
        assert!(
            answer.contains("not JSON"),
            "{body:?} was refused without a reason: {answer}",
        );
    }
}

/// EP-03, PF-46: the caller is admitted before the body is read at all. An unknown slug and
/// an endpoint that needs a token are refused with a body that could never have been parsed,
/// so neither is a way of making the gateway do work for a stranger.
#[tokio::test]
async fn nothing_of_the_body_is_read_before_the_caller_is_admitted() {
    let unparseable = "{".repeat(200 * 1024);
    for (slug, expected) in [
        ("zzzzzzzzzzzzzzzzzzzzzzzzzz", StatusCode::NOT_FOUND),
        (CLOSED, StatusCode::UNAUTHORIZED),
    ] {
        let (status, body) = call(check(slug, unparseable.clone())).await;
        assert_eq!(status, expected, "{slug}");
        assert!(!body.contains("decision"), "{slug} decided: {body}");
    }
}

/// T-0163: the body is read with its own ceiling, well under the one a write gets, because
/// nothing an access check needs is large. Past it the request is refused rather than held.
#[tokio::test]
async fn a_body_past_the_ceiling_is_refused_rather_than_read() {
    let padded = format!(
        r#"{{"action": {{"name": "queryEntity"}}, "pad": "{}"}}"#,
        "x".repeat(128 * 1024),
    );
    let (status, body) = call(check(OPEN, padded)).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(!body.contains("decision"), "{body}");
}

/// An action name that is not an operation is a no, not a 500 and not a yes. The list is the
/// ways a client gets the name wrong: a near miss, the wrong case, a prefixed spelling, an
/// empty string, and one with a newline that would otherwise reach a log line.
#[tokio::test]
async fn an_action_name_that_is_not_an_operation_is_a_no() {
    for action in [
        "",
        " ",
        "queryentity",
        "QUERYENTITY",
        "query_entity",
        "queryEntity ",
        " queryEntity",
        "queryEntity\r\nX-Injected: 1",
        "*",
        "../queryEntity",
        "\u{0}queryEntity",
        &"a".repeat(4096),
    ] {
        let (status, body) = call(check(OPEN, asking(action, None))).await;
        assert_eq!(status, StatusCode::OK, "{action:?} answered {body}");
        let answer: Value = serde_json::from_str(&body).expect("a decision");
        assert_eq!(answer["decision"], json!(false), "{action:?}");
    }
}

/// `resource.type` narrows the question, and a type that is not a string is the same as not
/// asking about a type at all: the check is over the operation, and a malformed narrowing
/// never widens the answer.
#[tokio::test]
async fn a_malformed_resource_type_never_widens_the_answer() {
    let unnarrowed: Value =
        serde_json::from_str(&call(check(OPEN, asking("queryEntity", None))).await.1)
            .expect("a decision");

    for resource in [
        json!({ "type": 42 }),
        json!({ "type": null }),
        json!({ "type": ["AirQualityObserved"] }),
        json!({ "Type": "AirQualityObserved" }),
        json!("AirQualityObserved"),
        json!(null),
    ] {
        let body = json!({ "action": { "name": "queryEntity" }, "resource": resource });
        let (status, answer) = call(check(OPEN, serde_json::to_vec(&body).expect("a query"))).await;
        assert_eq!(status, StatusCode::OK, "{body} answered {answer}");
        let decided: Value = serde_json::from_str(&answer).expect("a decision");
        assert_eq!(
            decided["decision"], unnarrowed["decision"],
            "{body} decided differently from the unnarrowed question",
        );
    }

    // And a type the caller may not touch is a no, whatever the operation says.
    let (_, refused) = call(check(OPEN, asking("queryEntity", Some("InternalIncident")))).await;
    let refused: Value = serde_json::from_str(&refused).expect("a decision");
    assert_eq!(refused["decision"], json!(false));
}

/// EP-55: the check is this caller's. A token that carries the steward's role decides
/// differently from the anonymous caller on the very same question, because it is the same
/// PDP answering about a different subject.
#[tokio::test]
async fn the_decision_is_the_callers_own() {
    let realm = common::Realm::new();
    let steward = realm.mint(&json!({
        "iss": common::ISSUER,
        "sub": "a-steward",
        "aud": CLOSED,
        "preferred_username": "jana",
        "groups": ["/ovzdusie"],
        "realm_access": { "roles": ["data-steward"] },
        "exp": common::in_seconds(300),
        "iat": common::in_seconds(-10),
    }));

    let (status, body) = call_on(
        app_of(&realm),
        Request::builder()
            .method("POST")
            .uri(format!("/api/endpoint/{CLOSED}/access/check"))
            .header(axum::http::header::CONTENT_TYPE, "application/json")
            .header(
                axum::http::header::AUTHORIZATION,
                format!("Bearer {steward}"),
            )
            .body(asking("createEntity", Some("InternalIncident")))
            .expect("a request"),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    let answer: Value = serde_json::from_str(&body).expect("a decision");
    assert_eq!(answer["decision"], json!(true), "{body}");
}

/// GW20: the tenancy headers are stripped before the endpoint is resolved, so a forged one
/// cannot move the question to another space's rules.
#[tokio::test]
async fn a_forged_tenant_header_does_not_change_the_decision() {
    let honest = call(check(OPEN, asking("createEntity", None))).await;
    let forged = call(
        Request::builder()
            .method("POST")
            .uri(format!("/api/endpoint/{OPEN}/access/check"))
            .header(axum::http::header::CONTENT_TYPE, "application/json")
            .header("NGSILD-Tenant", "somebody-elses-space")
            .header("X-Forwarded-Groups", "data-steward")
            .header("X-Forwarded-User", "spravca")
            .body(asking("createEntity", None))
            .expect("a request"),
    )
    .await;

    assert_eq!(honest.0, StatusCode::OK);
    assert_eq!(forged, honest);
}

/// The check is a `POST` surface, and nothing else reaches it — a `GET` in particular, which
/// would otherwise be a way of asking the question with the answer cached by a proxy.
#[tokio::test]
async fn nothing_but_post_reaches_the_check() {
    for method in ["GET", "PUT", "PATCH", "DELETE"] {
        let response = app()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(format!("/api/endpoint/{OPEN}/access/check"))
                    .body(Body::empty())
                    .expect("a request"),
            )
            .await
            .expect("the gateway answers");
        assert_eq!(
            response.status(),
            StatusCode::METHOD_NOT_ALLOWED,
            "{method} reached the check",
        );
    }
}
