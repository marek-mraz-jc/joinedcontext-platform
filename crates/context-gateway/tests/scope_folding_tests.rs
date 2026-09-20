//! Hierarchical scopes folded into the query as an anchored regex (T-0148, R12, R13, R30).

use chrono::{DateTime, Utc};
use context_gateway::pdp::evaluator::{evaluate, Request, Subject};
use context_gateway::pdp::scope_folding::fold;
use jc_core::kinds::{Operation, PolicySpec};
use regex::Regex;

fn now() -> DateTime<Utc> {
    "2026-09-06T12:00:00Z".parse().expect("a fixed instant")
}

fn policy(yaml: &str) -> PolicySpec {
    serde_norway::from_str(yaml).expect("the policy spec parses")
}

/// The regex inside a folded term, compiled the way a broker would compile it.
fn pattern(term: &str) -> Regex {
    let (_, rest) = term.split_once("scope~=\"").expect("a scope term");
    let (inner, _) = rest.split_once('"').expect("a closed pattern");
    Regex::new(inner).expect("the folded pattern compiles")
}

/// R30: a grant on the district covers the district and what is under it, and nothing
/// that merely starts with the same letters.
#[test]
fn children_match_and_siblings_and_prefixes_do_not() {
    let term = fold("/geo/SK/BB").expect("one path folds");
    assert_eq!(term, r#"scope~="^/geo/SK/BB(/.*)?$""#);

    let scope = pattern(&term);
    assert!(scope.is_match("/geo/SK/BB"));
    assert!(scope.is_match("/geo/SK/BB/Center"));
    assert!(scope.is_match("/geo/SK/BB/Center/Namestie"));

    assert!(!scope.is_match("/geo/SK/BA"), "a sibling district");
    assert!(
        !scope.is_match("/geo/SK/BBB"),
        "a longer name is not a child"
    );
    assert!(
        !scope.is_match("/geo/SK"),
        "the parent is not covered by the child"
    );
    assert!(!scope.is_match("x/geo/SK/BB"), "anchored at the start");
    assert!(
        !scope.is_match("/geo/SK/BB-old"),
        "the separator is a slash, nothing else"
    );
}

/// The scope query language separates paths with its own operators; a grant on two
/// districts is an OR of two anchored terms, and the wildcard suffix is the same subtree.
#[test]
fn several_paths_fold_to_an_or_and_wildcards_are_trimmed() {
    let term = fold("/geo/SK/BB/#|/geo/SK/ZA/").expect("two paths fold");
    assert_eq!(
        term,
        r#"(scope~="^/geo/SK/BB(/.*)?$"|scope~="^/geo/SK/ZA(/.*)?$")"#
    );
    assert_eq!(
        fold("(/geo/SK/BB;/geo/SK/BB/Center)")
            .expect("folds")
            .matches("scope~=")
            .count(),
        2
    );

    assert_eq!(fold(""), None);
    assert_eq!(
        fold("geo/SK/BB"),
        None,
        "a path starts with a slash or it is not one"
    );
    assert_eq!(fold("/"), None, "the root alone is not a grant on anything");
}

/// A scope name is a literal: the characters a regex would read as operators are escaped,
/// so `a.b` matches `a.b` and never `axb`.
#[test]
fn regex_operators_in_a_scope_name_are_literal() {
    let term = fold("/org/dept.a+b").expect("folds");
    let scope = pattern(&term);
    assert!(scope.is_match("/org/dept.a+b/team"));
    assert!(!scope.is_match("/org/deptxa+b"));
    assert!(!scope.is_match("/org/dept.ab"));
}

/// R12 and ADR 006: two policies with different scopes and different filters. Each
/// policy's filter and scope travel in one term, so the merged query can never pair one
/// policy's `q` with the other's scope.
#[test]
fn a_policys_filter_and_scope_stay_in_one_term() {
    let bb = policy(
        r#"contextSpaceRef: ovzdusie
assigner: did:web:banskabystrica.sk
assignee: { kind: role, id: public }
operations: [queryEntity]
information:
  - entities:
      - type: AirQualityObserved
q: "pm10>=0"
scopeQ: "/geo/SK/BB"
"#,
    );
    let za = policy(
        r#"contextSpaceRef: ovzdusie
assigner: did:web:banskabystrica.sk
assignee: { kind: role, id: public }
operations: [queryEntity]
information:
  - entities:
      - type: AirQualityObserved
q: "pm25>=0"
scopeQ: "/geo/SK/ZA"
"#,
    );

    let request = Request {
        q: Some("dateObserved>2026".to_owned()),
        ..Request::default()
    };
    let verdict = evaluate(
        &Subject::anonymous(),
        Operation::QueryEntity,
        &request,
        "ovzdusie",
        &[bb, za],
        now(),
    );
    let constraints = verdict.constraints().expect("a rewrite");

    assert_eq!(
        constraints.q.as_deref(),
        Some(concat!(
            "(dateObserved>2026);",
            r#"((((pm10>=0);scope~="^/geo/SK/BB(/.*)?$"))"#,
            "|",
            r#"(((pm25>=0);scope~="^/geo/SK/ZA(/.*)?$")))"#,
        ))
    );
    assert_eq!(
        constraints.granted_scopes.as_deref(),
        Some("/geo/SK/BB|/geo/SK/ZA"),
        "the write guard sees the union of the granted trees, and nothing the caller sent"
    );
}

/// R30: a caller's own `scopeQ` never reaches the granted scope tree. Sending
/// `scopeQ=/geo/SK/ZA` on a write against a grant on BB must not make ZA writable.
#[test]
fn the_callers_scope_never_widens_the_granted_tree() {
    let bb = policy(
        r#"contextSpaceRef: ovzdusie
assigner: did:web:banskabystrica.sk
assignee: { kind: role, id: public }
operations: [createEntity, queryEntity]
information:
  - entities:
      - type: AirQualityObserved
scopeQ: "/geo/SK/BB"
"#,
    );
    let request = Request {
        scope_q: Some("/geo/SK/ZA".to_owned()),
        ..Request::default()
    };
    let verdict = evaluate(
        &Subject::anonymous(),
        Operation::CreateEntity,
        &request,
        "ovzdusie",
        std::slice::from_ref(&bb),
        now(),
    );
    let constraints = verdict.constraints().expect("a rewrite");

    assert_eq!(constraints.granted_scopes.as_deref(), Some("/geo/SK/BB"));
    let entity = serde_json::json!({
        "id": "urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:ovzdusie:za-1",
        "type": "AirQualityObserved",
        "scope": "/geo/SK/ZA",
    });
    assert!(
        context_gateway::pdp::write_guard::check(
            &entity,
            constraints,
            "ovzdusie",
            "banskabystrica.sk"
        )
        .is_err(),
        "a write into a district the caller only named itself is refused"
    );
}
