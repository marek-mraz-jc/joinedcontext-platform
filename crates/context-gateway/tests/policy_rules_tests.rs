//! A Policy with `rules[]` is one grant per rule (ADR-N-057 decision 1, T-5036): the owner's
//! kiosk case, the public reads and one service account updates one entity, is one Policy, and a
//! prohibition rule of another bound Policy still wins (GW4).

use chrono::{DateTime, Utc};
use context_gateway::auth::subject::Subject;
use context_gateway::pdp::evaluator::{evaluate, Request, Verdict};
use context_gateway::resolver::Endpoint;
use context_gateway::store;
use jc_core::kinds::Operation;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

const KIOSK: &str = "urn:ngsi-ld:Kiosk:bbsk.sk:kiosks:hlavne-namestie";

const SPACE: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: ContextSpace
metadata:
  name: kiosks
  namespace: bbsk
spec:
  isSandbox: false
  urnSegment: kiosks
"#;

const BOUND: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: Endpoint
metadata:
  name: kiosks
  namespace: bbsk
spec:
  contextSpaceRef: kiosks
  slug: aaaaaaaaaaaaaaaaaaaaaaaaaa
  audience: public
  policyRef: urn:ngsi-ld:Policy:bbsk.sk:kiosks:kiosks
  enabledRepresentations: ["ngsi-ld"]
"#;

const WHOLE_SPACE: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: Endpoint
metadata:
  name: kiosks-all
  namespace: bbsk
spec:
  contextSpaceRef: kiosks
  slug: bbbbbbbbbbbbbbbbbbbbbbbbbb
  audience: public
  enabledRepresentations: ["ngsi-ld"]
"#;

const KIOSKS: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: Policy
metadata:
  name: kiosks
  namespace: bbsk
spec:
  contextSpaceRef: { kind: ContextSpace, name: kiosks }
  assigner: did:web:bbsk.sk
  rules:
    - assignee: { kind: role, id: public }
      operations: [retrieveOps]
      information:
        - entities: [{ type: Kiosk }]
    - assignee: { kind: serviceAccount, id: kiosk-editor }
      operations: [updateAttrs]
      information:
        - entities: [{ type: Kiosk, id: "urn:ngsi-ld:Kiosk:bbsk.sk:kiosks:hlavne-namestie" }]
"#;

const CLOSED: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: Policy
metadata:
  name: closed
  namespace: bbsk
spec:
  contextSpaceRef: { kind: ContextSpace, name: kiosks }
  assigner: did:web:bbsk.sk
  rules:
    - assignee: { kind: role, id: steward }
      operations: [retrieveOps]
    - effect: prohibition
      assignee: { kind: role, id: public }
      operations: [queryEntity]
      information:
        - entities: [{ type: Kiosk }]
"#;

fn now() -> DateTime<Utc> {
    "2026-10-22T12:00:00Z".parse().expect("a fixed instant")
}

fn repo(test_name: &str) -> PathBuf {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after the epoch")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("gw-policy-rules-{test_name}-{now}"));
    std::fs::create_dir_all(&dir).expect("create the repository");
    for (name, body) in [
        ("space.yaml", SPACE),
        ("bound.yaml", BOUND),
        ("all.yaml", WHOLE_SPACE),
        ("kiosks.yaml", KIOSKS),
        ("closed.yaml", CLOSED),
    ] {
        write(&dir, name, body);
    }
    dir
}

fn write(dir: &Path, name: &str, body: &str) {
    std::fs::write(dir.join(name), body).expect("write a manifest");
}

fn endpoint(dir: &Path, slug: &str) -> Endpoint {
    let (endpoints, ..) = store::load(dir).expect("the repository loads");
    endpoints
        .into_iter()
        .find(|endpoint| endpoint.slug == slug)
        .expect("the endpoint is in the table")
}

fn editor() -> Subject {
    Subject {
        service_account: Some("kiosk-editor".to_owned()),
        ..Subject::default()
    }
}

fn decide(subject: &Subject, operation: Operation, endpoint: &Endpoint) -> Verdict {
    evaluate(
        subject,
        operation,
        &Request::default(),
        "kiosks",
        &endpoint.policies,
        now(),
    )
}

#[test]
fn a_two_rule_policy_is_two_grants_named_by_rule() {
    let dir = repo("names");
    let bound = endpoint(&dir, "aaaaaaaaaaaaaaaaaaaaaaaaaa");
    assert_eq!(bound.policy_names, ["kiosks#1", "kiosks#2"]);
    assert_eq!(bound.policies.len(), 2);
    assert!(bound.policies.iter().all(|rule| rule.rules.is_none()));
}

#[test]
fn the_public_reads_and_one_account_updates_exactly_one_entity() {
    let dir = repo("grants");
    let bound = endpoint(&dir, "aaaaaaaaaaaaaaaaaaaaaaaaaa");

    let read = decide(&Subject::anonymous(), Operation::QueryEntity, &bound);
    let read = read.constraints().expect("the public reads");
    assert_eq!(read.types, BTreeSet::from(["Kiosk".to_owned()]));
    assert!(read.id_patterns.is_empty(), "{:?}", read.id_patterns);

    assert_eq!(
        decide(&Subject::anonymous(), Operation::UpdateAttrs, &bound),
        Verdict::Deny,
        "the public rule grants no write"
    );

    let update = decide(&editor(), Operation::UpdateAttrs, &bound);
    let update = update.constraints().expect("the editor updates");
    assert_eq!(
        update.id_patterns,
        BTreeSet::from([format!("^{}$", regex::escape(KIOSK))]),
        "one entity id and nothing else"
    );
    assert_eq!(
        decide(&editor(), Operation::DeleteEntity, &bound),
        Verdict::Deny,
        "only the operation the rule lists"
    );

    let other = Subject {
        service_account: Some("someone-else".to_owned()),
        ..Subject::default()
    };
    assert_eq!(
        decide(&other, Operation::UpdateAttrs, &bound),
        Verdict::Deny
    );
}

#[test]
fn a_prohibition_rule_of_another_bound_policy_wins() {
    let dir = repo("prohibition");
    let all = endpoint(&dir, "bbbbbbbbbbbbbbbbbbbbbbbbbb");
    let mut names = all.policy_names.clone();
    names.sort();
    assert_eq!(names, ["closed#1", "closed#2", "kiosks#1", "kiosks#2"]);
    assert_eq!(
        decide(&Subject::anonymous(), Operation::QueryEntity, &all),
        Verdict::Deny,
        "closed#2 prohibits what kiosks#1 grants"
    );
    assert!(
        decide(&editor(), Operation::UpdateAttrs, &all)
            .constraints()
            .is_some(),
        "the prohibition names the public role alone"
    );
}

#[test]
fn a_policy_with_neither_shape_grants_nothing() {
    let dir = repo("neither");
    let start = KIOSKS.find("  rules:\n").expect("rules");
    let neither = format!("{}  operations: [retrieveOps]\n", &KIOSKS[..start]);
    write(&dir, "kiosks.yaml", &neither);
    let bound = endpoint(&dir, "aaaaaaaaaaaaaaaaaaaaaaaaaa");
    assert!(bound.policies.is_empty(), "{:?}", bound.policy_names);
    assert_eq!(
        decide(&Subject::default(), Operation::QueryEntity, &bound),
        Verdict::Deny
    );
}

#[test]
fn a_policy_with_both_shapes_grants_nothing() {
    let dir = repo("both");
    write(
        &dir,
        "kiosks.yaml",
        &KIOSKS.replace(
            "  rules:\n",
            "  assignee: { kind: role, id: public }\n  operations: [deleteEntity]\n  rules:\n",
        ),
    );
    let bound = endpoint(&dir, "aaaaaaaaaaaaaaaaaaaaaaaaaa");
    assert!(bound.policies.is_empty(), "{:?}", bound.policy_names);
}
