//! Edge cases of `routes::data::handler` and `routes::data::endpoint_handler` (T-1863).
//!
//! The data route is the run's door to its project's context. Everything it is trusted with is
//! here: the run comes from the ticket, the endpoint comes from the run, the token is minted for
//! that endpoint alone, and nothing the workspace sets chooses any of the three.

mod common;

use axum::body::Body;
use axum::http::StatusCode;
use common::{app, authed, body_of, sample_run, Bases, RUN_ID, SLUG, TICKET};
use tower::ServiceExt;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A proxy whose gateway is `gateway` and whose other upstreams resolve nowhere.
fn with_gateway(allows_write: bool, gateway: &str) -> axum::Router {
    app(
        sample_run(allows_write),
        Bases {
            gateway: gateway.to_owned(),
            ..Bases::default()
        },
    )
}

/// AG-22, AG-64: no credential, no request to the gateway — the refusal happens in the proxy.
#[tokio::test]
async fn a_request_without_credentials_is_refused_before_the_gateway_is_asked() {
    let gateway = MockServer::start().await;
    let proxy = with_gateway(false, &gateway.uri());

    let response = proxy
        .oneshot(
            axum::http::Request::builder()
                .uri("/v1/data/ngsi-ld/v1/entities")
                .body(Body::empty())
                .expect("a request"),
        )
        .await
        .expect("an answer");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(
        gateway
            .received_requests()
            .await
            .unwrap_or_default()
            .is_empty(),
        "nothing reached the gateway"
    );
}

/// AG-22: an unknown run id and a wrong ticket answer the same sentence, so the route is no
/// oracle for which run ids are live (T-2285).
#[tokio::test]
async fn an_unknown_run_and_a_wrong_ticket_are_refused_with_the_same_sentence() {
    let mut details = Vec::new();
    for (run, ticket) in [
        (RUN_ID, "not-the-ticket"),
        ("11111111-2222-3333-4444-555555555555", TICKET),
        ("11111111-2222-3333-4444-555555555555", "not-the-ticket"),
    ] {
        let proxy = with_gateway(false, "http://context-gateway.invalid:8080");
        let response = proxy
            .oneshot(
                axum::http::Request::builder()
                    .uri("/v1/data/ngsi-ld/v1/entities")
                    .header("x-jc-run", run)
                    .header("x-jc-ticket", ticket)
                    .body(Body::empty())
                    .expect("a request"),
            )
            .await
            .expect("an answer");
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "run '{run}'");
        details.push(body_of(response).await);
    }
    assert!(
        details.windows(2).all(|pair| pair[0] == pair[1]),
        "the refusals differ and tell a caller which run ids exist: {details:?}"
    );
}

/// AG-22: the bearer form carries the same credential and is verified the same way; a bearer
/// naming another run is refused.
#[tokio::test]
async fn the_bearer_form_of_the_ticket_is_accepted_and_one_for_another_run_is_not() {
    let gateway = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string("[]"))
        .mount(&gateway)
        .await;

    for (bearer, expected) in [
        (format!("Bearer jcr_{RUN_ID}.{TICKET}"), StatusCode::OK),
        (
            format!("Bearer jcr_11111111-2222-3333-4444-555555555555.{TICKET}"),
            StatusCode::UNAUTHORIZED,
        ),
        (format!("Bearer jcr_{RUN_ID}."), StatusCode::UNAUTHORIZED),
        (format!("Bearer {TICKET}"), StatusCode::UNAUTHORIZED),
    ] {
        let proxy = with_gateway(false, &gateway.uri());
        let response = proxy
            .oneshot(
                axum::http::Request::builder()
                    .uri("/v1/data/ngsi-ld/v1/entities")
                    .header("authorization", &bearer)
                    .body(Body::empty())
                    .expect("a request"),
            )
            .await
            .expect("an answer");
        assert_eq!(response.status(), expected, "bearer '{bearer}'");
    }
}

/// AG-64, T-1300/T-1301: a path that would leave the endpoint's base is refused however it is
/// spelled — encoded once, encoded twice, a backslash, an absolute path, an empty segment.
#[tokio::test]
async fn a_path_that_would_leave_the_endpoint_is_refused_however_it_is_spelled() {
    let gateway = MockServer::start().await;
    for suffix in [
        "../admin",
        "%2e%2e/admin",
        "%252e%252e/admin",
        "ngsi-ld/v1/../../admin",
        "a//b",
        "./x",
    ] {
        let proxy = with_gateway(false, &gateway.uri());
        let response = proxy
            .oneshot(
                authed("GET", &format!("/v1/data/{suffix}"))
                    .body(Body::empty())
                    .expect("a request"),
            )
            .await
            .expect("an answer");
        assert!(
            response.status() == StatusCode::FORBIDDEN
                || response.status() == StatusCode::NOT_FOUND,
            "'{suffix}' answered {}",
            response.status()
        );
    }
    assert!(
        gateway
            .received_requests()
            .await
            .unwrap_or_default()
            .is_empty(),
        "no traversal reached the gateway"
    );
}

/// AG-64: `%2F` is decoded before the traversal check, so it names a segment separator and not a
/// way past it: `a%2Fb` is the ordinary path `a/b` under the run's own endpoint, and the check
/// runs on the decoded form rather than the one the workspace typed.
#[tokio::test]
async fn an_encoded_slash_is_a_segment_separator_under_the_runs_own_endpoint() {
    let gateway = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/api/endpoint/{SLUG}/a/b")))
        .respond_with(ResponseTemplate::new(200).set_body_string("[]"))
        .mount(&gateway)
        .await;

    let proxy = with_gateway(false, &gateway.uri());
    let response = proxy
        .oneshot(
            authed("GET", "/v1/data/a%2Fb")
                .body(Body::empty())
                .expect("a request"),
        )
        .await
        .expect("an answer");

    assert_eq!(response.status(), StatusCode::OK);
}

/// AG-64, AP-44: a slug of somebody else's endpoint is refused, and no token is minted for it.
#[tokio::test]
async fn an_endpoint_slug_the_run_does_not_name_is_refused() {
    let gateway = MockServer::start().await;
    let proxy = with_gateway(false, &gateway.uri());

    let response = proxy
        .oneshot(
            authed(
                "GET",
                "/v1/data/endpoints/someone-elses-slug/ngsi-ld/v1/entities",
            )
            .body(Body::empty())
            .expect("a request"),
        )
        .await
        .expect("an answer");

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let detail = body_of(response).await;
    assert!(
        detail.contains("someone-elses-slug") && detail.contains("not an endpoint of this run"),
        "the refusal names what was asked for: {detail}"
    );
    assert!(
        gateway
            .received_requests()
            .await
            .unwrap_or_default()
            .is_empty(),
        "nothing reached the gateway"
    );
}

/// AG-25: a read-only run writes nothing, whatever the method and whatever the path.
#[tokio::test]
async fn a_read_only_run_is_refused_every_write_method() {
    let gateway = MockServer::start().await;
    for verb in ["PATCH", "PUT", "DELETE"] {
        let proxy = with_gateway(false, &gateway.uri());
        let response = proxy
            .oneshot(
                authed(verb, "/v1/data/ngsi-ld/v1/entities/urn:ngsi-ld:Bike:a")
                    .body(Body::empty())
                    .expect("a request"),
            )
            .await
            .expect("an answer");
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{verb} passed");
    }
    assert!(
        gateway
            .received_requests()
            .await
            .unwrap_or_default()
            .is_empty(),
        "no write reached the gateway"
    );
}

/// AG-25: a POST that is a read (a query) passes on a read-only run; a POST that creates does not.
#[tokio::test]
async fn a_read_only_run_may_post_a_query_and_nothing_else() {
    let gateway = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_string("[]"))
        .mount(&gateway)
        .await;

    for (rest, expected) in [
        ("entityOperations/query", StatusCode::OK),
        ("ngsi-ld/v1/entityOperations/query", StatusCode::OK),
        ("mcp", StatusCode::OK),
        ("ngsi-ld/v1/entities", StatusCode::FORBIDDEN),
        ("entityOperations/upsert", StatusCode::FORBIDDEN),
        ("entityOperations/queryX", StatusCode::FORBIDDEN),
    ] {
        let proxy = with_gateway(false, &gateway.uri());
        let response = proxy
            .oneshot(
                authed("POST", &format!("/v1/data/{rest}"))
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .expect("a request"),
            )
            .await
            .expect("an answer");
        assert_eq!(response.status(), expected, "POST {rest}");
    }
}

/// AG-25: the MCP body of a read-only run is read for the tool it calls, so a mutation named
/// inside a POST that is otherwise allowed is refused too.
#[tokio::test]
async fn a_read_only_run_may_not_call_a_mutation_tool_over_mcp() {
    let gateway = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
        .mount(&gateway)
        .await;

    for (tool, expected) in [
        ("upsert_entity", StatusCode::FORBIDDEN),
        ("create_subscription", StatusCode::FORBIDDEN),
        ("query_entities", StatusCode::OK),
    ] {
        let proxy = with_gateway(false, &gateway.uri());
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": { "name": tool, "arguments": {} }
        })
        .to_string();
        let response = proxy
            .oneshot(
                authed("POST", "/v1/data/mcp")
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .expect("a request"),
            )
            .await
            .expect("an answer");
        assert_eq!(response.status(), expected, "tool {tool}");
    }
}

/// AG-64: the query string is the read, so it travels with the path and nothing of it is dropped.
#[tokio::test]
async fn the_whole_query_string_reaches_the_gateway() {
    let gateway = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/api/endpoint/{SLUG}/ngsi-ld/v1/entities")))
        .and(query_param("type", "Bike"))
        .and(query_param("limit", "3"))
        .and(query_param("options", "keyValues"))
        .respond_with(ResponseTemplate::new(200).set_body_string("[]"))
        .mount(&gateway)
        .await;

    let proxy = with_gateway(false, &gateway.uri());
    let response = proxy
        .oneshot(
            authed(
                "GET",
                "/v1/data/ngsi-ld/v1/entities?type=Bike&limit=3&options=keyValues",
            )
            .body(Body::empty())
            .expect("a request"),
        )
        .await
        .expect("an answer");

    assert_eq!(response.status(), StatusCode::OK);
}

/// AG-64: the credential the gateway sees is the endpoint's token and never the caller's. A
/// workspace that sets `Authorization`, a cookie or a gateway identity header changes nothing.
#[tokio::test]
async fn nothing_the_workspace_sets_reaches_the_gateway_as_an_identity() {
    let gateway = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string("[]"))
        .mount(&gateway)
        .await;

    let proxy = with_gateway(false, &gateway.uri());
    let response = proxy
        .oneshot(
            authed("GET", "/v1/data/ngsi-ld/v1/entities")
                .header("cookie", "session=stolen")
                .header("NgsiLD-Tenant", "another-tenant")
                .header("X-Userinfo", "{\"sub\":\"root\"}")
                .header("x-allowed-scope-ids", "*")
                .header("X-Endpoint-Slug", "someone-elses-slug")
                .header("x-consumer-identity", "root")
                .header("accept", "application/ld+json")
                .body(Body::empty())
                .expect("a request"),
        )
        .await
        .expect("an answer");
    assert_eq!(response.status(), StatusCode::OK);

    let seen = gateway
        .received_requests()
        .await
        .expect("the mock recorded");
    let headers = &seen
        .first()
        .expect("one request reached the gateway")
        .headers;
    assert_eq!(
        headers
            .get("authorization")
            .and_then(|value| value.to_str().ok()),
        Some(format!("Bearer mock-token-for-{SLUG}").as_str()),
        "the gateway is told the endpoint's token and nothing else"
    );
    for forbidden in [
        "cookie",
        "ngsild-tenant",
        "x-userinfo",
        "x-allowed-scope-ids",
        "x-endpoint-slug",
        "x-consumer-identity",
        "x-jc-run",
        "x-jc-ticket",
    ] {
        assert!(
            headers.get(forbidden).is_none(),
            "'{forbidden}' was relayed to the gateway"
        );
    }
    assert!(
        headers.get("accept").is_some(),
        "an ordinary header still travels"
    );
}

/// AG-41, T-0811: a body past the proxy's ceiling is refused, and the one at the ceiling is not.
#[tokio::test]
async fn a_body_past_the_ceiling_is_refused_and_the_one_at_it_is_not() {
    let gateway = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_string("[]"))
        .mount(&gateway)
        .await;
    let ceiling = 4 * 1024 * 1024;

    for (size, expected) in [
        (ceiling, StatusCode::OK),
        (ceiling + 1, StatusCode::PAYLOAD_TOO_LARGE),
    ] {
        let proxy = with_gateway(true, &gateway.uri());
        let response = proxy
            .oneshot(
                authed("POST", "/v1/data/entityOperations/query")
                    .header("content-type", "application/json")
                    .body(Body::from(vec![b'x'; size]))
                    .expect("a request"),
            )
            .await
            .expect("an answer");
        assert_eq!(response.status(), expected, "a body of {size} bytes");
    }
}

/// AG-64: a cookie the gateway sets is not a cookie the workspace gets.
#[tokio::test]
async fn a_set_cookie_from_the_gateway_is_not_relayed_to_the_workspace() {
    let gateway = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("set-cookie", "session=abc; HttpOnly")
                .insert_header("content-type", "application/json")
                .set_body_string("[]"),
        )
        .mount(&gateway)
        .await;

    let proxy = with_gateway(false, &gateway.uri());
    let response = proxy
        .oneshot(
            authed("GET", "/v1/data/ngsi-ld/v1/entities")
                .body(Body::empty())
                .expect("a request"),
        )
        .await
        .expect("an answer");

    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.headers().get("set-cookie").is_none());
    assert!(response.headers().get("content-type").is_some());
}

/// AG-64: every answer the gateway can give is relayed as itself, including the ones that are
/// not JSON and the ones that are errors — and none of them carries the gateway's address.
#[tokio::test]
async fn an_upstream_status_is_relayed_without_the_gateways_address() {
    for upstream in [401u16, 403, 404, 409, 429, 500] {
        let gateway = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(upstream).set_body_string("not json at all"))
            .mount(&gateway)
            .await;
        let host = gateway.address().to_string();

        let proxy = with_gateway(false, &gateway.uri());
        let response = proxy
            .oneshot(
                authed("GET", "/v1/data/ngsi-ld/v1/entities")
                    .body(Body::empty())
                    .expect("a request"),
            )
            .await
            .expect("an answer");

        assert_eq!(response.status().as_u16(), upstream);
        let body = body_of(response).await;
        assert_eq!(body, "not json at all", "the answer is relayed as it is");
        assert!(
            !body.contains(&host),
            "the gateway's address leaked: {body}"
        );
    }
}

/// AG-64: an unknown door of the proxy is refused by the fallback, not routed by accident.
#[tokio::test]
async fn a_door_the_proxy_does_not_have_is_refused() {
    for uri in ["/v1/admin", "/v1/data", "/internal/agent-runs/x", "/"] {
        let proxy = with_gateway(false, "http://context-gateway.invalid:8080");
        let response = proxy
            .oneshot(authed("GET", uri).body(Body::empty()).expect("a request"))
            .await
            .expect("an answer");
        assert!(
            response.status() == StatusCode::FORBIDDEN
                || response.status() == StatusCode::NOT_FOUND
                || response.status() == StatusCode::METHOD_NOT_ALLOWED,
            "{uri} answered {}",
            response.status()
        );
    }
}
