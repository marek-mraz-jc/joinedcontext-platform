//! An App job's token is the derived ServiceAccount `appjob-{app}` on the App's own Endpoint
//! (AP-159, AP-154, PF-46, T-3693).
//!
//! The job client `{project}-appjob-{app}` has no manifest: AP-159 refuses one. Resolved through
//! the ServiceAccount table alone it was nobody, and its token fell through to the human branch as
//! `user:service-account-…`, which no Policy grants. It is now the App's job principal, admitted
//! on the App's own Endpoint alone, holding that Endpoint's caller role and nothing else, and
//! never a person.

mod common;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use context_gateway::app::{router, Gateway};
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::store;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tower::ServiceExt;

const APP_SLUG: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaa";
const ALL_SLUG: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbb";
const JOB_CLIENT: &str = "helsinki-appjob-events";

const SPACE: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: ContextSpace
metadata:
  name: helsinki
  namespace: helsinki
spec:
  isSandbox: false
  urnSegment: helsinki
"#;

/// A wasm App, with the jobs given as YAML or none.
fn app(jobs: &str) -> String {
    format!(
        r#"apiVersion: joinedcontext.com/v1alpha1
kind: App
metadata:
  name: events
  namespace: helsinki
spec:
  kind: wasm
  source:
    git: {{ url: "https://dev.joinedcontext.com/git/joinedcontext/helsinki_events.git", ref: main }}
  build: {{ node: "22", rust: "1.90" }}
  visibility: project
  lifecycle: published
{jobs}  storage:
    sql: {{ migrations: db/migrations, quotaMiB: 50 }}
  dataNeeds:
    - contextSpaceRef: {{ kind: ContextSpace, name: helsinki }}
      types: [Event]
      operations: [queryEntity]
"#
    )
}

const JOBS: &str =
    "  server:\n    jobs:\n      - { name: daily, schedule: \"20 3 * * *\", export: run }\n";

/// The Endpoint the App reconciler generates for a `visibility: project` App, and a hand-written
/// one of the space with the same audience.
fn endpoint(name: &str, slug: &str, generated: bool) -> String {
    let annotations = if generated {
        "  annotations:\n    joinedcontext.com/generated-by: portal/app-reconciler\n"
    } else {
        ""
    };
    format!(
        r#"apiVersion: joinedcontext.com/v1alpha1
kind: Endpoint
metadata:
  name: {name}
  namespace: helsinki
{annotations}spec:
  contextSpaceRef: helsinki
  slug: {slug}
  audience: project-list
  allowedProjects: [helsinki]
  callerRole: {generated}
  enabledRepresentations: ["ngsi-ld"]
  roles:
    - name: editor
      subjects: [{{ user: "demo.steward@hel.fi" }}]
"#
    )
}

fn policy(name: &str, assignee: &str, entity_type: &str) -> String {
    format!(
        r#"apiVersion: joinedcontext.com/v1alpha1
kind: Policy
metadata:
  name: {name}
  namespace: helsinki
spec:
  contextSpaceRef: {{ kind: ContextSpace, name: helsinki }}
  assigner: did:web:hel.fi
  assignee: {assignee}
  effect: permission
  operations: [queryEntity]
  information:
    - entities:
        - type: {entity_type}
"#
    )
}

fn repo(test_name: &str, jobs: &str) -> PathBuf {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after the epoch")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("gw-appjob-{test_name}-{now}"));
    std::fs::create_dir_all(&dir).expect("create the repository");
    let files = [
        ("space.yaml", SPACE.to_owned()),
        ("app.yaml", app(jobs)),
        ("app-events.yaml", endpoint("app-events", APP_SLUG, true)),
        (
            "helsinki-all.yaml",
            endpoint("helsinki-all", ALL_SLUG, false),
        ),
        // What the App reconciler renders: the App's grant, to its Endpoint's caller role.
        (
            "p-app.yaml",
            policy(
                "app-events-1",
                "{ kind: role, id: \"endpoint:helsinki/app-events\" }",
                "Event",
            ),
        ),
        // A named role of the App's Endpoint: a person's, never the job's.
        (
            "p-app-editor.yaml",
            policy(
                "app-events-2-editor",
                "{ kind: role, id: \"endpoint:helsinki/app-events/editor\" }",
                "EventDraft",
            ),
        ),
        // The space's other grants: a realm role and the hand-written Endpoint's.
        (
            "p-steward.yaml",
            policy("steward-alerts", "{ kind: role, id: steward }", "Alert"),
        ),
    ];
    for (name, body) in files {
        write(&dir, name, &body);
    }
    dir
}

fn write(dir: &Path, name: &str, body: &str) {
    std::fs::write(dir.join(name), body).expect("write a manifest");
}

fn gateway(realm: &common::Realm, test_name: &str, jobs: &str) -> axum::Router {
    let (endpoints, _, accounts, ..) =
        store::load(&repo(test_name, jobs)).expect("the repository loads");
    router(Arc::new(
        Gateway::new(
            Broker::new("http://127.0.0.1:1"),
            Box::new(PolicyPdp),
            "hel.fi",
        )
        .serve(endpoints)
        .authenticate(Arc::new(realm.verifier()), accounts, None),
    ))
}

/// A token of the job client: its own service-account user, or a person's it exchanged.
fn token(realm: &common::Realm, user: &str) -> String {
    realm.mint(&json!({
        "iss": common::ISSUER,
        "sub": "f:1:job",
        "aud": "context-gateway",
        "azp": JOB_CLIENT,
        "preferred_username": user,
        "realm_access": { "roles": ["steward"] },
        "exp": common::in_seconds(300),
        "iat": common::in_seconds(-10),
    }))
}

/// `GET …/access` on `slug` with `token`: the status, and the permitted types when it is 200.
async fn access(app: &axum::Router, slug: &str, token: &str) -> (StatusCode, Vec<String>) {
    let request = Request::builder()
        .uri(format!("/api/endpoint/{slug}/access"))
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .expect("a request");
    let response = app
        .clone()
        .oneshot(request)
        .await
        .expect("the gateway answers");
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("a readable body");
    let document: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let mut types: Vec<String> = document["permissions"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|entry| Some(entry["resource"]["type"].as_str()?.to_owned()))
        .collect();
    types.sort();
    types.dedup();
    (status, types)
}

fn own_user() -> String {
    format!("service-account-{JOB_CLIENT}")
}

#[tokio::test]
async fn the_job_is_its_derived_account_with_the_caller_role_on_its_apps_endpoint() {
    let realm = common::Realm::new();
    let app = gateway(&realm, "own", JOBS);
    let (status, permitted) = access(&app, APP_SLUG, &token(&realm, &own_user())).await;
    assert_eq!(status, StatusCode::OK);
    // the caller role's grant; not the editor's, not the realm role the token claims
    assert_eq!(permitted, ["Event"]);
}

#[tokio::test]
async fn the_job_reaches_no_other_endpoint() {
    let realm = common::Realm::new();
    let app = gateway(&realm, "other", JOBS);
    let (status, _) = access(&app, ALL_SLUG, &token(&realm, &own_user())).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_persons_token_through_the_job_client_is_refused() {
    let realm = common::Realm::new();
    let app = gateway(&realm, "person", JOBS);
    let (status, _) = access(&app, APP_SLUG, &token(&realm, "demo.steward@hel.fi")).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "it delegates nothing");
}

#[tokio::test]
async fn an_app_without_jobs_has_no_job_client_and_its_token_is_nobody() {
    let realm = common::Realm::new();
    let app = gateway(&realm, "nojobs", "");
    for user in [own_user(), "demo.steward@hel.fi".to_owned()] {
        let (status, _) = access(&app, APP_SLUG, &token(&realm, &user)).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{user}: never a person");
    }
}

#[test]
fn the_table_resolves_the_job_client_only_while_the_app_runs_jobs() {
    use context_gateway::auth::accounts::client_id;
    use jc_core::kinds::app_identity::account_name;
    assert_eq!(client_id("helsinki", &account_name("events")), JOB_CLIENT);
    let (_, _, with_jobs, ..) = store::load(&repo("table", JOBS)).expect("the repository loads");
    let account = with_jobs.resolve(JOB_CLIENT).expect("the job principal");
    assert_eq!(
        (account.name.as_str(), account.project.as_str()),
        ("appjob-events", "helsinki")
    );
    assert!(account.roles.is_empty() && !account.delegates);
    assert!(with_jobs.admits_on(JOB_CLIENT, APP_SLUG));
    assert!(!with_jobs.admits_on(JOB_CLIENT, ALL_SLUG));
    let (_, _, without, ..) = store::load(&repo("table-none", "")).expect("the repository loads");
    assert!(without.resolve(JOB_CLIENT).is_none());
}
