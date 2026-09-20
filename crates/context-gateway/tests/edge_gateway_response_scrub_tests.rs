//! Edge cases of `middleware::response::scrub` (T-1906, EP-26, MP-02, R22, SP-05, T-2261).
//!
//! Contract, in one sentence: whatever the handler behind it said, no answer leaves carrying the
//! tenant, no answer carries the narrowing signal unless the request asked for it, and every
//! answer says it belongs to one caller so a shared cache cannot replay it to another.
//!
//! `response_headers_tests.rs` covers the tenant, the opt-in word and a repeated header. These
//! are the ones around them: the cache half, which no test touched, and the inputs a handler or a
//! prober can shape.

use axum::body::Body;
use axum::extract::Request;
use axum::http::{header, HeaderValue, Response, StatusCode};
use axum::routing::get;
use axum::Router;
use context_gateway::middleware::response::{
    asked_about_narrowing, scrub, RESULTS_RESTRICTED, WARNING,
};
use context_gateway::middleware::tenancy::TENANT;
use tower::ServiceExt;

/// A handler that answers with whatever a test hands it, so the layer is the only thing measured.
fn answering(build: fn(&mut Response<Body>)) -> Router {
    Router::new()
        .route(
            "/entities",
            get(move || async move {
                let mut response = Response::new(Body::from("[]"));
                build(&mut response);
                response
            }),
        )
        .layer(axum::middleware::from_fn(scrub))
}

fn asking(header_values: &[(&'static str, &'static str)]) -> Request {
    let mut builder = Request::builder().uri("/entities");
    for (name, value) in header_values {
        builder = builder.header(*name, *value);
    }
    builder.body(Body::empty()).expect("a request")
}

async fn through(build: fn(&mut Response<Body>), request: Request) -> Response<Body> {
    answering(build)
        .oneshot(request)
        .await
        .expect("the layer answers")
}

fn nothing(_: &mut Response<Body>) {}

#[tokio::test]
async fn every_answer_says_it_belongs_to_one_caller() {
    // R9, T-2261: two callers share a URL and are answered differently, so a shared cache that
    // stored one and replayed it to the other would serve an answer nobody decided.
    let response = through(nothing, asking(&[])).await;
    assert_eq!(
        response.headers()[header::CACHE_CONTROL],
        "private, no-store"
    );
    let vary = response.headers()[header::VARY]
        .to_str()
        .expect("a readable Vary");
    for named in ["Authorization", "Accept", "NGSILD-Results-Restricted"] {
        assert!(vary.contains(named), "Vary does not name {named}: {vary}");
    }
}

#[tokio::test]
async fn a_handler_that_asks_for_a_shared_cache_is_overruled() {
    // The handler is inside the trust boundary and can still be wrong; the layer is where the
    // rule is enforced, so a `public` it set is replaced rather than appended to.
    fn shared(response: &mut Response<Body>) {
        response.headers_mut().insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static("public, max-age=3600"),
        );
    }
    let response = through(shared, asking(&[])).await;
    assert_eq!(
        response.headers()[header::CACHE_CONTROL],
        "private, no-store"
    );
    assert_eq!(
        response
            .headers()
            .get_all(header::CACHE_CONTROL)
            .iter()
            .count(),
        1,
        "one directive, not the handler's and the layer's side by side"
    );
}

#[tokio::test]
async fn a_document_that_wants_revalidating_keeps_no_cache_and_gains_private() {
    // EP-51: the schema artifacts carry `no-cache` with a strong ETag, which is a cache asking
    // before it serves rather than a cache not storing at all. That distinction survives.
    fn revalidated(response: &mut Response<Body>) {
        response.headers_mut().insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static("no-cache, max-age=0"),
        );
    }
    let response = through(revalidated, asking(&[])).await;
    assert_eq!(
        response.headers()[header::CACHE_CONTROL],
        "private, no-cache"
    );
}

#[tokio::test]
async fn a_repeated_cache_control_leaves_one_value_and_it_is_the_layers() {
    fn twice(response: &mut Response<Body>) {
        let headers = response.headers_mut();
        headers.append(header::CACHE_CONTROL, HeaderValue::from_static("public"));
        headers.append(
            header::CACHE_CONTROL,
            HeaderValue::from_static("max-age=60"),
        );
    }
    let response = through(twice, asking(&[])).await;
    let values: Vec<_> = response
        .headers()
        .get_all(header::CACHE_CONTROL)
        .iter()
        .collect();
    assert_eq!(values.len(), 1, "a second value would be read by somebody");
    assert_eq!(values[0], "private, no-store");
}

#[tokio::test]
async fn the_tenant_is_gone_from_an_answer_that_is_not_a_success_either() {
    // A refusal is built by a different code path from a 200 and is just as much an answer.
    fn refused(response: &mut Response<Body>) {
        *response.status_mut() = StatusCode::FORBIDDEN;
        response
            .headers_mut()
            .insert(TENANT, HeaderValue::from_static("ovzdusie"));
    }
    let response = through(refused, asking(&[])).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert!(!response.headers().contains_key(&TENANT));
    assert_eq!(
        response.headers()[header::CACHE_CONTROL],
        "private, no-store"
    );
}

#[tokio::test]
async fn the_tenant_header_is_matched_however_the_handler_spelt_it() {
    // HTTP header names are case-insensitive, so a handler writing `NGSILD-Tenant` must not
    // survive a layer that removes `ngsild-tenant`.
    fn shouting(response: &mut Response<Body>) {
        response
            .headers_mut()
            .insert("NGSILD-TENANT", HeaderValue::from_static("ovzdusie"));
    }
    let response = through(shouting, asking(&[])).await;
    assert!(!response.headers().contains_key("ngsild-tenant"));
    assert!(!response.headers().contains_key("NGSILD-Tenant"));
}

#[tokio::test]
async fn the_warning_reaches_everybody_because_it_names_only_what_the_caller_may_read() {
    // T-1862: unlike the narrowing signal, the warning is not opt-in. It says which types the
    // caller may read were not queried, which is what turns an empty list a developer bisects
    // into one they can fix.
    fn warned(response: &mut Response<Body>) {
        response.headers_mut().insert(
            &WARNING,
            HeaderValue::from_static("199 - \"Vehicle was not selected\""),
        );
    }
    let response = through(warned, asking(&[])).await;
    assert!(
        response.headers().contains_key(&WARNING),
        "a caller who did not opt in still learns why their list is short"
    );
}

#[test]
fn only_the_word_true_opts_in_and_a_second_header_can_still_say_it() {
    // A caller may send the header twice; the layer reads every value, because a proxy that
    // appends its own must not silently switch the signal off.
    let one = Request::builder()
        .uri("/")
        .header(RESULTS_RESTRICTED.clone(), "false")
        .header(RESULTS_RESTRICTED.clone(), "true")
        .body(Body::empty())
        .expect("a request");
    assert!(asked_about_narrowing(&one));

    for value in ["", " true", "true ", "yes", "1", "truthy", "TRUE\u{0}"] {
        let Ok(header_value) = HeaderValue::from_str(value) else {
            continue;
        };
        let request = Request::builder()
            .uri("/")
            .header(RESULTS_RESTRICTED.clone(), header_value)
            .body(Body::empty())
            .expect("a request");
        assert!(
            !asked_about_narrowing(&request),
            "{value:?} is not the word true"
        );
    }
}

#[test]
fn a_value_that_is_not_text_at_all_does_not_opt_in() {
    // The comparison is over the raw bytes, so a value that is not UTF-8 is simply not "true"
    // rather than a decoding error on the request path.
    let request = Request::builder()
        .uri("/")
        .header(
            RESULTS_RESTRICTED.clone(),
            HeaderValue::from_bytes(&[0xff, 0xfe]).expect("a legal header value"),
        )
        .body(Body::empty())
        .expect("a request");
    assert!(!asked_about_narrowing(&request));
}

#[tokio::test]
async fn asking_for_the_signal_does_not_buy_the_tenant_with_it() {
    fn both(response: &mut Response<Body>) {
        let headers = response.headers_mut();
        headers.insert(TENANT, HeaderValue::from_static("ovzdusie"));
        headers.insert(&RESULTS_RESTRICTED, HeaderValue::from_static("true"));
    }
    let response = through(both, asking(&[("NGSILD-Results-Restricted", "true")])).await;
    assert!(response.headers().contains_key(&RESULTS_RESTRICTED));
    assert!(
        !response.headers().contains_key(&TENANT),
        "the opt-in is about narrowing, not about internal names"
    );
}
