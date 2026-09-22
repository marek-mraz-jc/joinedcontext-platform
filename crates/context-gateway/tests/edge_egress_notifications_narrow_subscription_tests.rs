//! Edge cases of `egress::notifications::narrow_subscription` (T-1902, EP-26, MP-02).
//!
//! Contract, in one sentence: what is stored in the broker is a subscription that can only
//! ever fire for granted types, carry granted and unhidden attributes, match the grants' own
//! filter as well as the subscriber's, and deliver through this gateway with the granted areas
//! written into the target — and anything it cannot narrow faithfully it refuses, because a
//! subscription is a read that happens later, when nobody is holding the caller's token
//! (GW27, R46, R9, R12).
//!
//! The two shapes matter and are asserted separately. A `POST` carries the whole subscription,
//! so what it leaves out is filled in from the grants; a `PATCH` carries a fragment, and what
//! it leaves out was narrowed when it was stored — filling that in from the grants would
//! replace the subscriber's own selection with the widest one the grants allow.
//!
//! `notification_egress_tests.rs` drives all of this through the gateway and owns the delivery
//! half. These are the cases at the function's own edges: the payload that is not an object,
//! each member missing in turn under both shapes, a selector with no type, an unbalanced
//! filter, and every refusal the routing makes.

use context_gateway::egress::notifications::narrow_subscription;
use context_gateway::pdp::evaluator::Constraints;
use context_gateway::resolver::Endpoint;
use jc_core::kinds::{Audience, Representation};
use serde_json::{json, Value};
use std::collections::BTreeSet;

const SLUG: &str = "d7m2xq9vkt4zc6wrb8shj5nfp3";
const BASE: &str = "https://2.28.67.127.sslip.io";

fn set(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

fn endpoint(hidden: &[&str]) -> Endpoint {
    Endpoint {
        roles: Default::default(),
        slug: SLUG.to_owned(),
        title: std::collections::BTreeMap::new(),
        description: std::collections::BTreeMap::new(),
        space: "ovzdusie".to_owned(),
        project: "banskabystrica".to_owned(),
        audience: Audience::Public,
        allowed_projects: Vec::new(),
        representations: vec![Representation::NgsiLd],
        rate_limit: None,
        file_limits: None,
        hidden_attributes: set(hidden),
        projection: None,
        base_path: format!("/api/endpoint/{SLUG}"),
        models: Vec::new(),
        view_mapping: None,
        policies: Vec::new(),
    }
}

/// The grants of a caller who may watch `AirQualityObserved` and read two of its attributes.
fn granted() -> Constraints {
    Constraints {
        tenant: "ovzdusie".to_owned(),
        types: set(&["AirQualityObserved"]),
        attrs: set(&["temperature", "pm10"]),
        ..Constraints::default()
    }
}

/// A subscription as a subscriber writes one.
fn subscription(uri: &str) -> Value {
    json!({
        "type": "Subscription",
        "entities": [{ "type": "AirQualityObserved" }],
        "notification": { "endpoint": { "uri": uri, "accept": "application/json" } },
    })
}

/// Narrows a `POST`, with no hidden attribute and a public target.
fn created(mut value: Value) -> Result<Value, Box<jc_core::ProblemDetails>> {
    narrow_subscription(&mut value, &granted(), &endpoint(&[]), BASE, &[], true).map(|()| value)
}

/// Narrows a `PATCH` of the same shape.
fn patched(mut value: Value) -> Result<Value, Box<jc_core::ProblemDetails>> {
    narrow_subscription(&mut value, &granted(), &endpoint(&[]), BASE, &[], false).map(|()| value)
}

#[test]
fn a_subscription_that_is_not_an_object_is_refused_before_anything_is_read() {
    for not_an_object in [
        json!(null),
        json!([]),
        json!("subscription"),
        json!(7),
        json!(true),
    ] {
        let problem = created(not_an_object.clone()).expect_err("not a subscription");
        assert_eq!(problem.status, 400, "{not_an_object} is not an object");
    }
}

/// A creation that names no type would stand for every type in the tenant, which is wider than
/// the grant — so the grant's own types become the selector. A `PATCH` that names none is
/// saying nothing about the selector, and what is stored was narrowed already.
#[test]
fn a_creation_with_no_selector_takes_the_grants_and_a_patch_with_none_is_left_alone() {
    let mut without = subscription("https://subscriber.example/hook");
    without
        .as_object_mut()
        .expect("an object")
        .remove("entities");

    let stored = created(without.clone()).expect("it is narrowed");
    assert_eq!(
        stored["entities"],
        json!([{ "type": "AirQualityObserved" }])
    );

    let fragment = patched(without).expect("the fragment is narrowed");
    assert!(
        fragment.get("entities").is_none(),
        "a PATCH that says nothing about the selector does not gain one",
    );
}

#[test]
fn a_selector_naming_a_type_that_is_not_granted_is_dropped_and_all_of_them_is_a_refusal() {
    let mut mixed = subscription("https://subscriber.example/hook");
    mixed["entities"] = json!([
        { "type": "AirQualityObserved" },
        { "type": "Vehicle" },
        { "type": "airqualityobserved" },
    ]);
    let stored = created(mixed).expect("one of them is granted");
    assert_eq!(
        stored["entities"],
        json!([{ "type": "AirQualityObserved" }]),
        "the type is matched exactly, so the lower-case one is another type",
    );

    let mut none = subscription("https://subscriber.example/hook");
    none["entities"] = json!([{ "type": "Vehicle" }]);
    assert_eq!(created(none).expect_err("nothing granted").status, 403);
}

/// A selector that names no type at all — an `idPattern` alone — selects every type in the
/// tenant, so it is not a selector the grants can narrow: it is dropped, and a subscription
/// whose every selector is dropped is refused rather than stored watching nothing.
#[test]
fn a_selector_with_no_type_is_not_narrowable_and_is_dropped() {
    let mut by_id = subscription("https://subscriber.example/hook");
    by_id["entities"] = json!([{ "idPattern": "^urn:ngsi-ld:AirQualityObserved:.*" }]);
    assert_eq!(created(by_id).expect_err("no type to narrow").status, 403);

    let mut beside = subscription("https://subscriber.example/hook");
    beside["entities"] = json!([
        { "idPattern": "^urn:.*" },
        { "type": "AirQualityObserved", "idPattern": "^urn:ngsi-ld:AirQualityObserved:.*" },
    ]);
    let stored = beside.clone();
    let stored = created(stored).expect("one selector survives");
    assert_eq!(stored["entities"].as_array().expect("a list").len(), 1);
    assert_eq!(stored["entities"][0]["type"], json!("AirQualityObserved"));
}

#[test]
fn the_watched_attributes_are_narrowed_to_the_grants_and_an_ungranted_list_is_refused() {
    let mut watching = subscription("https://subscriber.example/hook");
    watching["watchedAttributes"] = json!(["temperature", "salary", "pm10"]);
    let stored = created(watching).expect("two of them are granted");
    assert_eq!(stored["watchedAttributes"], json!(["pm10", "temperature"]));

    let mut none = subscription("https://subscriber.example/hook");
    none["watchedAttributes"] = json!(["salary"]);
    assert_eq!(created(none).expect_err("nothing granted").status, 403);

    // An empty list asks for nothing, and `narrow` reads an empty request as "everything
    // granted" (evaluator.rs:438) — so `watchedAttributes: []` is stored as the two granted
    // attributes rather than as nothing. It is wider than what the subscriber wrote and never
    // wider than the grant, so the subscription fires more often than asked and still carries
    // only granted attributes. Asserted as it is, and written up in T-1902.
    let mut empty = subscription("https://subscriber.example/hook");
    empty["watchedAttributes"] = json!([]);
    assert_eq!(
        created(empty).expect("nothing asked")["watchedAttributes"],
        json!(["pm10", "temperature"]),
    );
}

/// EP-61: an attribute the endpoint publishes nothing of is not watchable, whatever the
/// grants say — and a subscription that watches only hidden attributes is refused.
#[test]
fn an_endpoints_hidden_attribute_is_never_watched_or_notified() {
    let mut value = subscription("https://subscriber.example/hook");
    value["watchedAttributes"] = json!(["temperature", "pm10"]);
    value["notification"]["attributes"] = json!(["temperature", "pm10"]);

    narrow_subscription(
        &mut value,
        &granted(),
        &endpoint(&["pm10"]),
        BASE,
        &[],
        true,
    )
    .expect("temperature is still granted");
    assert_eq!(value["watchedAttributes"], json!(["temperature"]));
    assert_eq!(value["notification"]["attributes"], json!(["temperature"]));

    let mut only_hidden = subscription("https://subscriber.example/hook");
    only_hidden["watchedAttributes"] = json!(["pm10"]);
    let problem = narrow_subscription(
        &mut only_hidden,
        &granted(),
        &endpoint(&["pm10"]),
        BASE,
        &[],
        true,
    )
    .expect_err("nothing left to watch");
    assert_eq!(problem.status, 403);
}

/// R9: the grant is a whitelist, so a notification that names no attributes carries the
/// whitelist rather than whatever the entity happens to hold when it fires.
#[test]
fn a_notification_that_names_no_attributes_is_given_the_granted_ones() {
    let stored = created(subscription("https://subscriber.example/hook")).expect("narrowed");
    assert_eq!(
        stored["notification"]["attributes"],
        json!(["pm10", "temperature"]),
    );

    // And with no attribute grant there is nothing to whitelist, so nothing is inserted.
    let mut value = subscription("https://subscriber.example/hook");
    let wide = Constraints {
        tenant: "ovzdusie".to_owned(),
        types: set(&["AirQualityObserved"]),
        ..Constraints::default()
    };
    narrow_subscription(&mut value, &wide, &endpoint(&[]), BASE, &[], true).expect("narrowed");
    assert!(value["notification"].get("attributes").is_none());
}

/// Every member the routing needs is required of a creation and optional in a fragment: a
/// `PATCH` that does not mention the notification is not saying the subscription has none.
#[test]
fn a_creation_needs_a_notification_endpoint_and_a_fragment_does_not() {
    let mut no_notification = subscription("https://subscriber.example/hook");
    no_notification
        .as_object_mut()
        .expect("an object")
        .remove("notification");
    assert_eq!(
        created(no_notification.clone())
            .expect_err("no endpoint")
            .status,
        400,
    );
    assert!(patched(no_notification).is_ok());

    let mut no_endpoint = subscription("https://subscriber.example/hook");
    no_endpoint["notification"] = json!({ "attributes": ["temperature"] });
    assert_eq!(
        created(no_endpoint.clone())
            .expect_err("no endpoint")
            .status,
        400
    );
    assert!(patched(no_endpoint).is_ok());

    let mut no_uri = subscription("https://subscriber.example/hook");
    no_uri["notification"]["endpoint"] = json!({ "accept": "application/json" });
    assert_eq!(created(no_uri.clone()).expect_err("no uri").status, 400);
    assert!(patched(no_uri).is_ok());

    // A notification that is not an object is the same as none: a creation is refused.
    let mut not_an_object = subscription("https://subscriber.example/hook");
    not_an_object["notification"] = json!("hook");
    assert_eq!(created(not_an_object).expect_err("no endpoint").status, 400);
}

/// The stored target is this gateway, with the subscriber's own address carried as an
/// encoded parameter: that is what makes the broker deliver through the projection instead of
/// straight to the subscriber.
#[test]
fn the_stored_target_is_this_gateway_and_carries_the_subscribers_address_encoded() {
    let stored =
        created(subscription("https://subscriber.example/hook?x=1&y=2")).expect("narrowed");
    let uri = stored["notification"]["endpoint"]["uri"]
        .as_str()
        .expect("a uri");

    assert!(
        uri.starts_with(&format!("{BASE}/api/endpoint/{SLUG}")),
        "{uri}"
    );
    assert!(
        uri.contains("to=https%3A%2F%2Fsubscriber.example%2Fhook%3Fx%3D1%26y%3D2"),
        "{uri}"
    );
    assert!(
        !uri.contains("subscriber.example/hook?x=1"),
        "the address is a parameter, not a second path: {uri}",
    );
}

/// The areas the grants drew are written into the stored target, so the delivery path filters
/// against them even though nobody is holding the caller's token then (GW11).
#[test]
fn the_granted_areas_are_written_into_the_stored_target() {
    let mut value = subscription("https://subscriber.example/hook");
    let geo = Constraints {
        geo_grants: vec!["near;maxDistance==2000;point;[24.9,60.2]".to_owned()],
        ..granted()
    };
    narrow_subscription(&mut value, &geo, &endpoint(&[]), BASE, &[], true).expect("narrowed");

    let uri = value["notification"]["endpoint"]["uri"]
        .as_str()
        .expect("a uri");
    assert!(uri.contains("area=near%3BmaxDistance%3D%3D2000"), "{uri}");

    // With no grant of its own, the `geoQ` the broker would have been given is the area.
    let mut fallback = subscription("https://subscriber.example/hook");
    let from_q = Constraints {
        geo_q: Some("within;polygon;[[0,0],[0,1],[1,1],[0,0]]".to_owned()),
        ..granted()
    };
    narrow_subscription(&mut fallback, &from_q, &endpoint(&[]), BASE, &[], true).expect("narrowed");
    assert!(fallback["notification"]["endpoint"]["uri"]
        .as_str()
        .expect("a uri")
        .contains("area=within%3Bpolygon"),);
}

/// What may not be stored as a target: something that is not HTTP at all, something already
/// pointing at the gateway's own egress path (which would deliver to itself for ever), and an
/// address inside the platform's networks.
#[test]
fn a_target_the_gateway_will_not_deliver_to_is_refused_at_creation() {
    for uri in [
        "mqtt://subscriber.example/topic",
        "ftp://subscriber.example/hook",
        "file:///etc/passwd",
        "javascript:alert(1)",
        "",
        "//subscriber.example/hook",
    ] {
        let problem = created(subscription(uri)).expect_err("not deliverable");
        assert_eq!(problem.status, 400, "{uri} is not an HTTP target");
    }

    let loop_back =
        format!("{BASE}/api/endpoint/{SLUG}/egress/notifications?to=https%3A%2F%2Fx.example");
    assert_eq!(
        created(subscription(&loop_back))
            .expect_err("a loop")
            .status,
        400,
    );

    for inside in [
        "http://localhost:8080/hook",
        "http://sub.localhost/hook",
        "http://127.0.0.1/hook",
        "http://10.1.2.3/hook",
        "http://169.254.169.254/latest/meta-data/",
        "http://[::1]/hook",
    ] {
        let problem = created(subscription(inside)).expect_err("inside the platform");
        assert_eq!(problem.status, 400, "{inside} is inside the platform");
    }
}

/// A host the installation itself named is let through, because that is how an in-cluster
/// subscriber — another workload of this platform — is subscribed at all.
#[test]
fn a_host_the_installation_names_is_let_through_although_it_is_private() {
    let mut value = subscription("http://reporting.jc.svc.cluster.local:8080/hook");
    narrow_subscription(
        &mut value,
        &granted(),
        &endpoint(&[]),
        BASE,
        &["reporting.jc.svc.cluster.local".to_owned()],
        true,
    )
    .expect("the installation named it");
    assert!(value["notification"]["endpoint"]["uri"]
        .as_str()
        .expect("a uri")
        .contains("to=http%3A%2F%2Freporting.jc.svc.cluster.local%3A8080%2Fhook"));
}

/// A gateway that does not know its own address cannot route a delivery back through itself,
/// and says so with 501 rather than storing a subscription that would deliver straight to the
/// subscriber unprojected.
#[test]
fn a_gateway_that_knows_no_address_of_its_own_refuses_rather_than_storing_a_direct_delivery() {
    let mut value = subscription("https://subscriber.example/hook");
    let problem = narrow_subscription(&mut value, &granted(), &endpoint(&[]), "", &[], true)
        .expect_err("nowhere to route through");
    assert_eq!(problem.status, 501);
}

/// R12/R13: the grants' filter is conjoined with the subscriber's own, and a `PATCH` that says
/// nothing about `q` leaves the stored one — which was narrowed when it was stored — alone.
#[test]
fn the_grants_filter_is_conjoined_on_creation_and_a_silent_patch_keeps_what_was_stored() {
    let filtered = Constraints {
        q: Some("category==\"air\"".to_owned()),
        ..granted()
    };

    let mut own = subscription("https://subscriber.example/hook");
    own["q"] = json!("temperature>10");
    narrow_subscription(&mut own, &filtered, &endpoint(&[]), BASE, &[], true).expect("narrowed");
    assert_eq!(own["q"], json!("(temperature>10);category==\"air\""));

    let mut none = subscription("https://subscriber.example/hook");
    narrow_subscription(&mut none, &filtered, &endpoint(&[]), BASE, &[], true).expect("narrowed");
    assert_eq!(
        none["q"],
        json!("category==\"air\""),
        "a creation with no filter of its own gets the grants'",
    );

    let mut fragment = subscription("https://subscriber.example/hook");
    narrow_subscription(&mut fragment, &filtered, &endpoint(&[]), BASE, &[], false)
        .expect("narrowed");
    assert!(
        fragment.get("q").is_none(),
        "a PATCH that says nothing about `q` does not gain one",
    );

    let mut changed = subscription("https://subscriber.example/hook");
    changed["q"] = json!("temperature>20");
    narrow_subscription(&mut changed, &filtered, &endpoint(&[]), BASE, &[], false)
        .expect("narrowed");
    assert_eq!(changed["q"], json!("(temperature>20);category==\"air\""));
}

/// ADR 006: a filter whose parentheses do not balance would consume the wrapping and regroup
/// what follows, so it is dropped rather than conjoined — the grants' filter stands alone and
/// the subscription is narrower, never wider.
#[test]
fn a_filter_whose_parentheses_do_not_balance_is_dropped_and_never_widens_the_grants() {
    let filtered = Constraints {
        q: Some("category==\"air\"".to_owned()),
        ..granted()
    };
    for unbalanced in ["temperature>10)", "(temperature>10", "))", "("] {
        let mut value = subscription("https://subscriber.example/hook");
        value["q"] = json!(unbalanced);
        narrow_subscription(&mut value, &filtered, &endpoint(&[]), BASE, &[], true)
            .expect("narrowed");
        assert_eq!(
            value["q"],
            json!("category==\"air\""),
            "{unbalanced:?} is dropped, and the grants' filter stands",
        );
    }
}

/// Narrowing the same payload twice is narrowing it once: the second pass finds an already
/// routed target and refuses it rather than wrapping it a second time, so a retry cannot
/// build a chain of gateways pointing at each other.
#[test]
fn narrowing_an_already_narrowed_subscription_is_refused_rather_than_wrapped_again() {
    let stored = created(subscription("https://subscriber.example/hook")).expect("narrowed");
    let problem = created(stored).expect_err("already routed");
    assert_eq!(problem.status, 400);
}
