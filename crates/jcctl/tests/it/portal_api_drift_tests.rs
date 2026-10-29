//! T-5966: jcctl's Portal routes against the Portal OpenAPI, vendored in `portal-api.json` by
//! `scripts/vendor-portal-api.py` at a pinned Portal commit. A path, method or query parameter the
//! command table uses that the Portal no longer serves, or a kind the Portal serves that jcctl
//! cannot name, fails here.

use jcctl::portal::{OPS, ROUTES};
use serde_json::Value;
use std::collections::BTreeSet;

fn vendored() -> Value {
    serde_json::from_str(include_str!("../../portal-api.json")).expect("portal-api.json is JSON")
}

#[test]
fn every_route_jcctl_calls_is_in_the_vendored_openapi_with_its_query_parameters() {
    let api = vendored();
    let op_route = format!("{OPS}/{{name}}");
    for route in ROUTES {
        if route.path == op_route {
            // One path per operation in the document; their names are checked below.
            assert_eq!(route.method, "POST");
            continue;
        }
        let served = api["paths"][route.path][route.method]
            .as_array()
            .unwrap_or_else(|| {
                panic!(
                    "{} {} ({}) is not in the Portal OpenAPI",
                    route.method, route.path, route.verb
                )
            });
        for parameter in route.query {
            assert!(
                served.iter().any(|p| p == parameter),
                "{} {} has no query parameter {parameter}",
                route.method,
                route.path
            );
        }
    }
}

#[test]
fn every_kind_the_portal_serves_is_a_kind_jcctl_names() {
    let api = vendored();
    let served: BTreeSet<&str> = api["kinds"]
        .as_array()
        .expect("kinds")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(!served.is_empty());
    let known: BTreeSet<&str> = jc_core::KINDS.iter().map(|k| k.kind).collect();
    let missing: Vec<&&str> = served.difference(&known).collect();
    assert!(
        missing.is_empty(),
        "the Portal serves kinds jcctl cannot name: {missing:?}"
    );
}

#[test]
fn every_operation_is_reachable_by_op_and_names_its_verb() {
    let api = vendored();
    let ops = api["ops"].as_array().expect("ops");
    assert!(ops.len() > 1);
    for op in ops {
        let name = op["name"].as_str().expect("a name");
        assert!(
            !name.is_empty() && name.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
            "{name} is not one path segment jcctl op can send"
        );
        assert!(op["verb"].is_string(), "{name} names no required verb");
    }
    assert!(ROUTES
        .iter()
        .any(|r| r.verb == "op" && r.path == format!("{OPS}/{{name}}")));
    assert!(
        api["paths"][OPS]["GET"].is_array(),
        "GET {OPS} is not served"
    );
}

#[test]
fn the_extract_names_the_portal_commit_and_file_it_came_from() {
    let api = vendored();
    let hex = |field: &str, len: usize| {
        let value = api["source"][field].as_str().unwrap_or("");
        value.len() == len && value.chars().all(|c| c.is_ascii_hexdigit())
    };
    assert!(hex("commit", 40) && hex("sha256", 64));
    assert_eq!(api["source"]["file"], "ui/openapi.json");
}
