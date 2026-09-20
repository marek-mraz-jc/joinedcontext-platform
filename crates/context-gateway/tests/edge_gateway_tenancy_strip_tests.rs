//! Edge cases of `middleware::tenancy::strip_client_headers` (T-1907, EP-21, EP-26, GW20, GW25).
//!
//! Contract, in one sentence: after this function, no header a downstream component would believe
//! about tenancy or identity is one the client wrote, whatever case the client spelt it in and
//! however many copies it sent — and nothing else the client wrote is touched.
//!
//! `tenancy_middleware_tests.rs` covers the forged tenant, a repeated tenant, the whole forgeable
//! list and one ordinary header surviving, all through the router. These are the function's own
//! edges: over-stripping, spelling, and the list staying in step with the header it protects.

use axum::body::Body;
use axum::extract::Request;
use axum::http::HeaderValue;
use context_gateway::middleware::tenancy::{pin_tenant, strip_client_headers, FORGEABLE, TENANT};

fn with(headers: &[(&str, &str)]) -> Request {
    let mut builder = Request::builder().uri("/api/endpoint/s/ngsi-ld/v1/entities");
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    builder.body(Body::empty()).expect("a request")
}

#[test]
fn the_header_the_gateway_pins_is_one_the_client_cannot() {
    // If `TENANT` were ever renamed without `FORGEABLE` following, a client could set the new
    // name and the broker would believe it. This is the one assertion that ties them together.
    assert!(
        FORGEABLE.contains(&TENANT.as_str()),
        "the tenant header the gateway pins is not in the list of headers a client may not set"
    );
}

#[test]
fn every_forgeable_name_is_lower_case_because_that_is_how_it_is_matched() {
    for name in FORGEABLE {
        assert_eq!(
            *name,
            name.to_ascii_lowercase(),
            "{name} would never match a header map keyed in lower case"
        );
    }
}

#[test]
fn a_client_cannot_hide_a_forged_header_behind_its_spelling() {
    // HTTP header names are case-insensitive. A client sending `X-UserInfo` must not survive a
    // stripper looking for `x-userinfo`.
    let mut request = with(&[
        ("NGSILD-TENANT", "doprava"),
        ("X-UserInfo", "e30="),
        ("X-Access-Token", "forged"),
        ("X-Allowed-Scope-Ids", "1"),
        ("X-Endpoint-Slug", "other"),
        ("X-Consumer-Identity", "somebody"),
    ]);
    strip_client_headers(&mut request);
    assert!(request.headers().is_empty(), "{:?}", request.headers());
}

#[test]
fn a_repeated_forgeable_header_leaves_no_copy_for_whoever_reads_the_last_one() {
    let mut request = Request::builder().uri("/");
    for value in ["a", "b", "c"] {
        request = request.header("x-userinfo", value);
    }
    let mut request = request.body(Body::empty()).expect("a request");
    strip_client_headers(&mut request);
    assert_eq!(request.headers().get_all("x-userinfo").iter().count(), 0);
}

#[test]
fn a_name_that_merely_looks_forgeable_is_left_alone() {
    // Over-stripping is its own defect: a header removed by accident is a feature that stops
    // working for a reason nobody can see. The match is the whole name, never a prefix.
    let kept = [
        ("x-userinfo-extra", "1"),
        ("xx-userinfo", "2"),
        ("userinfo", "3"),
        ("ngsild-tenant-hint", "4"),
        ("x-access-token-id", "5"),
    ];
    let mut request = with(&kept);
    strip_client_headers(&mut request);
    assert_eq!(request.headers().len(), kept.len());
    for (name, value) in kept {
        assert_eq!(request.headers()[name], value, "{name} was removed");
    }
}

#[test]
fn the_headers_a_request_needs_to_be_answered_at_all_survive() {
    let mut request = with(&[
        ("authorization", "Bearer t"),
        ("accept", "application/ld+json"),
        ("content-type", "application/json"),
        ("if-match", "\"7f3a\""),
        ("x-forwarded-for", "203.0.113.7"),
        ("ngsild-tenant", "forged"),
    ]);
    strip_client_headers(&mut request);
    assert_eq!(request.headers().len(), 5);
    assert!(!request.headers().contains_key(&TENANT));
    assert_eq!(request.headers()["authorization"], "Bearer t");
    assert_eq!(request.headers()["if-match"], "\"7f3a\"");
}

#[test]
fn a_request_with_no_headers_at_all_is_not_a_special_case() {
    let mut request = Request::builder()
        .uri("/")
        .body(Body::empty())
        .expect("a request");
    strip_client_headers(&mut request);
    assert!(request.headers().is_empty());
}

#[test]
fn stripping_twice_is_the_same_as_stripping_once() {
    // The layer runs before routing; a handler that calls it again must not have to care.
    let mut request = with(&[("x-userinfo", "e30="), ("accept", "application/json")]);
    strip_client_headers(&mut request);
    strip_client_headers(&mut request);
    assert_eq!(request.headers().len(), 1);
    assert_eq!(request.headers()["accept"], "application/json");
}

#[test]
fn a_forged_value_that_is_not_text_is_removed_like_any_other() {
    // A client can send bytes that are not UTF-8; the stripper works on names, so it must not
    // depend on the value being readable.
    let mut request = Request::builder()
        .uri("/")
        .header(
            "x-userinfo",
            // High bytes are legal in a header value; a NUL is not, so this is what a client
            // can really send and the stripper really has to survive.
            HeaderValue::from_bytes(&[0xff, 0xfe]).expect("a legal header value"),
        )
        .body(Body::empty())
        .expect("a request");
    strip_client_headers(&mut request);
    assert!(!request.headers().contains_key("x-userinfo"));
}

#[test]
fn the_tenant_the_gateway_pins_replaces_anything_left_rather_than_joining_it() {
    // `pin_tenant` runs after the strip, but it must not rely on it: an `insert` leaves one
    // value, an `append` would leave a client's beside the gateway's.
    let mut request = Request::builder()
        .uri("/")
        .header(TENANT, "forged")
        .body(Body::empty())
        .expect("a request");
    pin_tenant(&mut request, "ovzdusie").expect("a DNS-1123 label is a legal header value");
    let values: Vec<_> = request.headers().get_all(&TENANT).iter().collect();
    assert_eq!(values, vec!["ovzdusie"]);
}

#[test]
fn a_space_name_that_is_not_a_legal_header_value_pins_nothing() {
    // A name with CR/LF would split the header on the hop to the broker; one with a NUL or a
    // control character is refused for the same reason. The reconciler should never produce one,
    // and if it does this pins nothing rather than something wrong.
    for illegal in ["ovzdusie\r\nX-Access-Token: forged", "a\0b", "a\nb", "a\rb"] {
        let mut request = Request::builder()
            .uri("/")
            .body(Body::empty())
            .expect("a request");
        assert!(
            pin_tenant(&mut request, illegal).is_err(),
            "{illegal:?} was accepted as a tenant"
        );
        assert!(
            !request.headers().contains_key(&TENANT),
            "{illegal:?} left a tenant behind"
        );
    }
}

#[test]
fn a_space_name_with_no_characters_in_it_still_pins_nothing_useful() {
    // The empty string is a legal header value, so this is not an error; it is recorded because
    // an empty tenant is a broker's default tenant, and a reader should know which it is.
    let mut request = Request::builder()
        .uri("/")
        .body(Body::empty())
        .expect("a request");
    pin_tenant(&mut request, "").expect("the empty string is a legal header value");
    assert_eq!(request.headers()[&TENANT], "");
}
