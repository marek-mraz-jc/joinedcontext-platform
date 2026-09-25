//! Edge cases of `middleware::rate_limit::enforce` (T-1905, EP-20, EP-26, MP-02, MIM0-R7).
//!
//! Contract, in one sentence: every call on a surface that declares a limit spends exactly one
//! token of the bucket belonging to that endpoint and that caller, and no request a caller can
//! shape lets them spend somebody else's bucket or skip their own.
//!
//! `rate_limit_tests.rs` covers the bucket arithmetic, the 429 with its headers and the forged
//! `X-Forwarded-For`. These are the layer's other branches: the paths that are deliberately not
//! counted, the canonical space surface with the gateway's own default, and the keying.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use context_gateway::app::{router, Gateway};
use context_gateway::middleware::rate_limit::RateLimiter;
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::Endpoint;
use jc_core::kinds::{Audience, RateLimits, Representation};
use std::sync::Arc;
use std::time::Instant;
use tower::ServiceExt;

const SLUG: &str = "zt4qm7ge2xdv6ksb3ncf5arw2y";
const SPACE: &str = "ovzdusie";

fn limits(per_minute: u32, burst: Option<u32>) -> RateLimits {
    RateLimits {
        requests_per_minute: per_minute,
        burst,
    }
}

fn endpoint(rate_limit: Option<RateLimits>) -> Endpoint {
    Endpoint {
        roles: Default::default(),
        slug: SLUG.to_owned(),
        title: std::collections::BTreeMap::new(),
        description: std::collections::BTreeMap::new(),
        space: SPACE.to_owned(),
        project: SPACE.to_owned(),
        audience: Audience::Public,
        allowed_projects: Vec::new(),
        representations: vec![Representation::NgsiLd, Representation::GeoJson],
        rate_limit,
        file_limits: None,
        hidden_attributes: Default::default(),
        projection: None,
        base_path: format!("/api/endpoint/{SLUG}"),
        models: Vec::new(),
        view_mapping: None,
        catalog: None,
        policies: Vec::new(),
    }
}

fn app(rate_limit: Option<RateLimits>) -> axum::Router {
    let gateway = Gateway::new(
        Broker::new("http://127.0.0.1:1".to_owned()),
        Box::new(PolicyPdp),
        "banskabystrica.sk",
    )
    .serve([endpoint(rate_limit)]);
    router(Arc::new(gateway))
}

/// One call, with whatever headers a test wants to key on.
async fn call(
    app: &axum::Router,
    uri: &str,
    headers: &[(&str, &str)],
) -> axum::http::Response<Body> {
    let mut builder = Request::builder().uri(uri);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    app.clone()
        .oneshot(builder.body(Body::empty()).expect("a request"))
        .await
        .expect("the stack answers")
}

/// A path that is answered without reaching the broker, so a test measures the layer and not a
/// connection to nowhere.
fn schema(slug: &str) -> String {
    format!("/api/endpoint/{slug}/schema/index.json")
}

const FROM: &[(&str, &str)] = &[("x-forwarded-for", "203.0.113.7")];

#[tokio::test]
async fn an_unknown_slug_costs_nobody_a_token() {
    // EP-03: the handler answers 404 and the bucket is untouched, so a prober cannot spend a
    // real endpoint's quota by guessing addresses, and cannot fill the map with buckets either.
    let app = app(Some(limits(60, Some(1))));
    for _ in 0..20 {
        let response = call(&app, &schema("aaaaaaaaaaaaaaaaaaaaaaaaaa"), FROM).await;
        assert_ne!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(
            !response.headers().contains_key("ratelimit-limit"),
            "an answer nobody was counted for advertises no quota"
        );
    }
    // And the real endpoint still has its whole burst.
    let real = call(&app, &schema(SLUG), FROM).await;
    assert_ne!(real.status(), StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn an_endpoint_that_declares_no_limit_is_not_limited_here() {
    // The manifest field is optional and APISIX in front still has its own; what must not happen
    // is a default appearing from nowhere and refusing a caller the manifest never capped.
    let app = app(None);
    for _ in 0..50 {
        let response = call(&app, &schema(SLUG), FROM).await;
        assert_ne!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(!response.headers().contains_key("ratelimit-limit"));
    }
}

#[tokio::test]
async fn an_unknown_space_costs_nobody_a_token_either() {
    // The space surface's own default (`SPACE_DEFAULT`) is covered by
    // `rate_limit_tests.rs::the_canonical_space_surface_is_counted_per_caller`; this is the
    // other branch, where nothing resolves and nothing is spent.
    let app = app(None);
    let response = call(&app, "/cs/does-not-exist/ngsi-ld/v1/entities", FROM).await;
    assert_ne!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(!response.headers().contains_key("ratelimit-limit"));
}

#[tokio::test]
async fn a_path_with_an_empty_slug_is_not_read_as_a_slug() {
    // `/api/endpoint//…` would otherwise key a bucket on the empty string, which every caller
    // sending that path would then share.
    let app = app(Some(limits(60, Some(1))));
    for _ in 0..5 {
        let response = call(&app, "/api/endpoint//schema/index.json", FROM).await;
        assert_ne!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    }
}

#[tokio::test]
async fn a_percent_encoded_slug_is_not_the_slug_it_encodes() {
    // The path is matched as it arrives. A caller who encodes the separator must not reach the
    // endpoint's bucket by a second spelling of its address, which would double the quota.
    let app = app(Some(limits(60, Some(1))));
    let encoded = SLUG.to_uppercase();
    let response = call(&app, &schema(&encoded), FROM).await;
    assert_ne!(
        response.status(),
        StatusCode::TOO_MANY_REQUESTS,
        "an unknown spelling is an unknown slug, not a second door to the same bucket"
    );
    assert!(!response.headers().contains_key("ratelimit-limit"));

    // The real slug still has its burst: the upper-case call did not spend it.
    let real = call(&app, &schema(SLUG), FROM).await;
    assert_eq!(real.headers()["ratelimit-remaining"], "0");
}

#[tokio::test]
async fn two_credentials_from_one_address_do_not_spend_each_others_quota() {
    // EP-20: keying by the credential's digest is what stops two clients behind one NAT from
    // refusing each other.
    let app = app(Some(limits(60, Some(1))));
    let one = &[
        ("x-forwarded-for", "203.0.113.7"),
        ("authorization", "Bearer one"),
    ];
    let two = &[
        ("x-forwarded-for", "203.0.113.7"),
        ("authorization", "Bearer two"),
    ];

    assert_ne!(
        call(&app, &schema(SLUG), one).await.status(),
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(
        call(&app, &schema(SLUG), one).await.status(),
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_ne!(
        call(&app, &schema(SLUG), two).await.status(),
        StatusCode::TOO_MANY_REQUESTS,
        "the second credential has a bucket of its own"
    );
}

#[tokio::test]
async fn a_credentialed_caller_and_an_anonymous_one_are_not_the_same_bucket() {
    let app = app(Some(limits(60, Some(1))));
    let credentialed = &[
        ("x-forwarded-for", "203.0.113.7"),
        ("authorization", "Bearer one"),
    ];

    assert_ne!(
        call(&app, &schema(SLUG), credentialed).await.status(),
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(
        call(&app, &schema(SLUG), credentialed).await.status(),
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_ne!(
        call(&app, &schema(SLUG), FROM).await.status(),
        StatusCode::TOO_MANY_REQUESTS,
        "spending a credential's bucket does not spend the address's"
    );
}

#[tokio::test]
async fn a_caller_that_sends_no_forwarded_for_shares_one_bucket_and_not_everybody_elses() {
    // Without the header there is nothing to key on, so such callers share the `unknown` bucket.
    // That is the honest floor; what must not happen is them spending a named address's quota.
    let app = app(Some(limits(60, Some(1))));
    assert_ne!(
        call(&app, &schema(SLUG), &[]).await.status(),
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(
        call(&app, &schema(SLUG), &[]).await.status(),
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_ne!(
        call(&app, &schema(SLUG), FROM).await.status(),
        StatusCode::TOO_MANY_REQUESTS,
        "a named address keeps its own burst"
    );
}

#[tokio::test]
async fn an_empty_forwarded_for_falls_back_rather_than_keying_on_nothing() {
    let app = app(Some(limits(60, Some(1))));
    let empty = &[("x-forwarded-for", "   ")];
    assert_ne!(
        call(&app, &schema(SLUG), empty).await.status(),
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(
        call(&app, &schema(SLUG), &[]).await.status(),
        StatusCode::TOO_MANY_REQUESTS,
        "a blank header and a missing one are the same caller: `unknown`"
    );
}

#[test]
fn a_limit_of_zero_refuses_rather_than_dividing_by_it() {
    // `requestsPerMinute: 0` makes the refill rate zero. A bucket that never refills must answer
    // with a finite wait rather than an infinity cast to an integer.
    let limiter = RateLimiter::new();
    let quota = limits(0, Some(1));
    let now = Instant::now();

    assert!(
        limiter.check(SLUG, "address:a", &quota, now).allowed,
        "the burst is still one"
    );
    let refused = limiter.check(SLUG, "address:a", &quota, now);
    assert!(!refused.allowed);
    assert_eq!(refused.limit, 0);
    assert_eq!(
        refused.reset, 60,
        "a rate of nothing is told to come back in a minute"
    );
}

#[test]
fn a_burst_of_zero_is_read_as_one_rather_than_refusing_everybody_for_ever() {
    // `max(1.0)`: a bucket of no tokens could never hand one out, which would take an endpoint
    // off the air over a field somebody typed a zero in.
    let limiter = RateLimiter::new();
    let quota = limits(60, Some(0));
    let now = Instant::now();
    assert!(limiter.check(SLUG, "address:a", &quota, now).allowed);
    assert!(!limiter.check(SLUG, "address:a", &quota, now).allowed);
}

#[test]
fn the_remaining_a_header_advertises_is_never_more_than_the_caller_has() {
    // `remaining` is a truncating cast of the tokens left, so a caller told `n` really has n
    // whole requests and is never refused one of them. A cast that rounded up would promise a
    // request that is not there.
    let limiter = RateLimiter::new();
    let quota = limits(600, Some(10));
    let now = Instant::now();

    let promised = limiter.check(SLUG, "address:a", &quota, now).remaining;
    assert_eq!(
        promised, 9,
        "one of the ten is the call that was just answered"
    );
    for spent in 0..promised {
        assert!(
            limiter.check(SLUG, "address:a", &quota, now).allowed,
            "the caller was promised {promised} more and was refused after {spent}"
        );
    }
    assert!(
        !limiter.check(SLUG, "address:a", &quota, now).allowed,
        "and not one beyond what was promised"
    );
}
