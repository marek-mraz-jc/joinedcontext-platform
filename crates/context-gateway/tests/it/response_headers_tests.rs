//! What the gateway concluded for itself never leaves in a header (T-0941, T-0942, SP-05, R22).
//!
//! Two rules, one layer: the tenant is pinned for the hop to the broker and never comes back
//! out, and the narrowing signal is answered only to a caller who asked for it. The layer is
//! tested here on its own, and through the real router in `space_surface_tests`, which is
//! what proves it is actually wired in.

use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderValue, StatusCode};
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use context_gateway::app::Gateway;
use context_gateway::middleware::response::{scrub, RESULTS_RESTRICTED};
use context_gateway::middleware::tenancy::TENANT;
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use std::sync::Arc;
use tower::ServiceExt;

/// A handler that answers the way the broker and the projection stage do: with both headers
/// set, because that is where the knowledge lives.
async fn answers_with_both() -> Response<Body> {
    let mut response = Response::new(Body::from("[]"));
    response
        .headers_mut()
        .insert(TENANT, HeaderValue::from_static("ovzdusie"));
    response
        .headers_mut()
        .insert(RESULTS_RESTRICTED, HeaderValue::from_static("true"));
    response
}

fn app() -> Router {
    Router::new()
        .route("/entities", get(answers_with_both))
        .layer(axum::middleware::from_fn_with_state(
            // The layer needs the resolver for the `describedby` link of a data path; this
            // one is `/entities`, which is not an endpoint surface (EP-50).
            Arc::new(Gateway::new(
                Broker::new("http://127.0.0.1:1".to_owned()),
                Box::new(PolicyPdp),
                "hel.fi",
            )),
            scrub,
        ))
}

async fn answer(request: Request) -> Response<Body> {
    app().oneshot(request).await.expect("the layer answers")
}

fn asking(value: Option<&'static str>) -> Request {
    let mut builder = Request::builder().uri("/entities");
    if let Some(value) = value {
        builder = builder.header("NGSILD-Results-Restricted", value);
    }
    builder.body(Body::empty()).expect("a request")
}

/// SP-05: the tenant is an internal name and a probe for which spaces exist. No answer
/// carries it, whatever the handler behind the layer said.
#[tokio::test]
async fn no_answer_carries_the_tenant() {
    let response = answer(asking(None)).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        !response.headers().contains_key(&TENANT),
        "the answer handed the caller the internal tenant name"
    );
}

/// R22, GW12: a caller who did not ask is answered as if the result were simply what it is.
#[tokio::test]
async fn the_narrowing_signal_is_not_volunteered() {
    let response = answer(asking(None)).await;
    assert!(
        !response.headers().contains_key(&RESULTS_RESTRICTED),
        "a caller who never asked was told the answer was narrowed"
    );
}

/// R22: a caller who asks is told.
#[tokio::test]
async fn the_narrowing_signal_is_answered_to_whoever_asked() {
    let response = answer(asking(Some("true"))).await;
    assert_eq!(
        response.headers().get(&RESULTS_RESTRICTED),
        Some(&HeaderValue::from_static("true")),
        "the caller asked to be told about narrowing and was not"
    );
    assert!(
        !response.headers().contains_key(&TENANT),
        "asking about narrowing is not asking for the tenant"
    );
}

/// The opt-in is the value `true`, spelled however the caller spells it; anything else is
/// not an opt-in, so the signal stays off.
#[tokio::test]
async fn the_opt_in_is_the_word_true_and_nothing_else() {
    for (value, asked) in [
        ("TRUE", true),
        ("True", true),
        ("false", false),
        ("1", false),
    ] {
        let response = answer(asking(Some(value))).await;
        assert_eq!(
            response.headers().contains_key(&RESULTS_RESTRICTED),
            asked,
            "`NGSILD-Results-Restricted: {value}` was read as {asked}"
        );
    }
}

/// A repeated header leaves no copy behind for whoever reads the last one.
#[tokio::test]
async fn a_repeated_header_is_removed_entirely() {
    async fn twice() -> Response<Body> {
        let mut response = Response::new(Body::empty());
        let headers = response.headers_mut();
        headers.append(TENANT, HeaderValue::from_static("ovzdusie"));
        headers.append(TENANT, HeaderValue::from_static("doprava"));
        headers.append(RESULTS_RESTRICTED, HeaderValue::from_static("true"));
        headers.append(RESULTS_RESTRICTED, HeaderValue::from_static("true"));
        response
    }
    let response = Router::new()
        .route("/entities", get(twice))
        .layer(axum::middleware::from_fn_with_state(
            Arc::new(Gateway::new(
                Broker::new("http://127.0.0.1:1".to_owned()),
                Box::new(PolicyPdp),
                "hel.fi",
            )),
            scrub,
        ))
        .oneshot(asking(None))
        .await
        .expect("the layer answers");
    assert_eq!(response.headers().get_all(&TENANT).iter().count(), 0);
    assert_eq!(
        response
            .headers()
            .get_all(&RESULTS_RESTRICTED)
            .iter()
            .count(),
        0
    );
}
