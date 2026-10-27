//! Attack vector: a person or workload of one Organization calls another Organization's
//! `audience: organization` Endpoint (T-5557; EP-14, ADR-N-058 2.4, Architecture/04 §3).
//!
//! One installation serves several Organizations from one realm, each with its own group subtree
//! (ADR-N-058). An `organization` audience admits the members of the Endpoint's own Organization:
//! a person whose groups name one of its projects, or a ServiceAccount of one of them. A caller of
//! another Organization holds a valid token of the same realm, so the token alone says nothing;
//! the gateway reads each project's Organization from the Project manifests (`Provenance`).
//!
//! The outsider is answered exactly as a slug nobody created is, so the answer does not tell an
//! Endpoint of another Organization apart from one that does not exist (PF-32, R20).

mod common;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use context_gateway::app::{router, Gateway};
use context_gateway::auth::accounts::{accounts_of, client_id};
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::Endpoint;
use context_gateway::store::Provenance;
use jc_core::kinds::{Audience, PolicySpec, Representation};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;
use tower::ServiceExt;

const SLUG: &str = "h7bikesopsq2mzt6vhx3nbwrs5";
/// A slug no manifest declares, the answer the outsider must not be told apart from.
const NOBODYS: &str = "n0bodysq2mzt6vhx3nbwrs5cjd";
/// The Endpoint's project, of Organization `hel`.
const PROJECT: &str = "helsinki-mobility";
/// Another project of `hel`.
const SIBLING: &str = "helsinki";
/// The project of Organization `zilina`, named like its subtree root.
const OUTSIDER: &str = "zilina";

fn endpoint() -> Endpoint {
    let policy: PolicySpec = serde_norway::from_str(
        "contextSpaceRef: bikes\nassigner: did:web:hel.fi\n\
         assignee: { kind: role, id: member }\noperations: [queryEntity]\n\
         information:\n  - entities:\n      - type: BikeHireDockingStation\n",
    )
    .expect("the policy spec parses");
    Endpoint {
        declared_types: None,
        roles: Default::default(),
        slug: SLUG.to_owned(),
        title: Default::default(),
        description: Default::default(),
        space: "bikes".to_owned(),
        project: PROJECT.to_owned(),
        audience: Audience::Organization,
        allowed_projects: Vec::new(),
        representations: vec![Representation::NgsiLd],
        rate_limit: None,
        creates: None,
        file_limits: None,
        hidden_attributes: Default::default(),
        projection: None,
        base_path: format!("/api/endpoint/{SLUG}"),
        models: Vec::new(),
        view_mapping: None,
        catalog: None,
        deprecation: None,
        feeds: Vec::new(),
        calendars: Vec::new(),
        map_defaults: Default::default(),
        policy_names: Vec::new(),
        manifest_name: String::new(),
        policies: vec![policy],
    }
}

/// A repository holding the ServiceAccount `senzory` in each of `projects`.
struct Repo(PathBuf);

impl Repo {
    fn new(name: &str, projects: &[&str]) -> Self {
        let dir = std::env::temp_dir().join(format!("gateway-attack-organization-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        for project in projects {
            let accounts = dir.join(format!("projects/{project}/access/serviceaccounts"));
            std::fs::create_dir_all(&accounts).expect("an account directory");
            std::fs::write(
                accounts.join("senzory.yaml"),
                format!("apiVersion: joinedcontext.com/v1alpha1\nkind: ServiceAccount\nmetadata:\n  name: senzory\n  namespace: {project}\nspec:\n  owner:\n    user: demo.steward\n  purpose: \"the attack\"\n  roles:\n    - role: member\n      scope: {{ project: {project} }}\n  credentials:\n    - kind: oauth-client\n      name: default\n"),
            )
            .expect("the account");
        }
        Self(dir)
    }
}

impl Drop for Repo {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Fixture {
    realm: common::Realm,
    app: axum::Router,
    broker: common::BrokerStub,
    _repo: Repo,
}

async fn fixture(name: &str) -> Fixture {
    fixture_of(name, true).await
}

/// The fixture, with every project's Organization known or, before ADR-N-058, none.
async fn fixture_of(name: &str, organizations_known: bool) -> Fixture {
    let realm = common::Realm::new();
    let broker = common::BrokerStub::start(vec![json!([])]).await;
    let repo = Repo::new(name, &[PROJECT, OUTSIDER]);
    let accounts =
        accounts_of(&jc_manifest::loader::Repository::load(&repo.0).expect("the repository loads"));
    let gateway = Gateway::new(
        Broker::new(broker.url.clone()),
        Box::new(PolicyPdp),
        "hel.fi",
    )
    .serve([endpoint()])
    .authenticate(Arc::new(realm.verifier()), accounts, None);
    if organizations_known {
        gateway.replace_provenance(Provenance {
            organizations: [(PROJECT, "hel"), (SIBLING, "hel"), (OUTSIDER, "zilina")]
                .into_iter()
                .map(|(project, organization)| (project.to_owned(), organization.to_owned()))
                .collect(),
            ..Default::default()
        });
    }
    Fixture {
        realm,
        app: router(Arc::new(gateway)),
        broker,
        _repo: repo,
    }
}

fn person(realm: &common::Realm, slug: &str, groups: &[&str]) -> String {
    realm.mint(&json!({
        "iss": common::ISSUER,
        "sub": "f:1:someone",
        "aud": slug,
        "exp": common::in_seconds(300),
        "iat": common::in_seconds(-10),
        "preferred_username": "demo.someone",
        "groups": groups,
        "realm_access": { "roles": ["member"] },
    }))
}

/// `GET {slug}/ngsi-ld/v1/entities` with the token: the status and the body.
async fn read(fixture: &Fixture, slug: &str, token: &str) -> (StatusCode, Value) {
    let response = fixture
        .app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/api/endpoint/{slug}/ngsi-ld/v1/entities?type=BikeHireDockingStation"
                ))
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .expect("a request"),
        )
        .await
        .expect("the gateway answers");
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("a readable body");
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

/// EP-14, ADR-N-058: a Žilina person reaches nothing of Helsinki's organization Endpoint, and
/// the answer is the one a slug nobody created gets.
#[tokio::test]
async fn a_person_of_another_organization_is_not_admitted_to_an_organization_endpoint() {
    let fixture = fixture("another-person").await;
    let token = person(&fixture.realm, SLUG, &[OUTSIDER]);
    let (status, body) = read(&fixture, SLUG, &token).await;
    let nobodys = person(&fixture.realm, NOBODYS, &[OUTSIDER]);
    let (missing_status, missing_body) = read(&fixture, NOBODYS, &nobodys).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(
        (status, body),
        (missing_status, missing_body),
        "the outsider is told what a missing slug tells"
    );
    assert_eq!(fixture.broker.hops().len(), 0, "the broker was never asked");
}

/// A person whose groups name no project at all is no member of any Organization.
#[tokio::test]
async fn a_person_of_no_project_is_not_admitted_to_an_organization_endpoint() {
    let fixture = fixture("no-project").await;
    let token = person(&fixture.realm, SLUG, &["platform-readers"]);
    let (status, body) = read(&fixture, SLUG, &token).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(fixture.broker.hops().len(), 0, "the broker was never asked");
}

/// The control: a person of a sibling project of the same Organization is admitted, and so is
/// one of the Endpoint's own project.
#[tokio::test]
async fn a_person_of_the_same_organization_is_admitted_to_an_organization_endpoint() {
    let fixture = fixture("same-organization").await;
    for groups in [[SIBLING], [PROJECT]] {
        let token = person(&fixture.realm, SLUG, &groups);
        let (status, body) = read(&fixture, SLUG, &token).await;
        assert_eq!(status, StatusCode::OK, "{groups:?}: {body}");
    }
    assert_eq!(
        fixture.broker.hops().len(),
        2,
        "both reads reached the broker"
    );
}

/// A ServiceAccount of another Organization's project is refused the same way; one of the
/// Endpoint's own Organization is admitted.
#[tokio::test]
async fn a_workload_of_another_organization_is_not_admitted_to_an_organization_endpoint() {
    let fixture = fixture("another-workload").await;
    let outsider = fixture
        .realm
        .workload_token(&client_id(OUTSIDER, "senzory"), json!(SLUG));
    let (status, body) = read(&fixture, SLUG, &outsider).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(fixture.broker.hops().len(), 0, "the broker was never asked");
    let member = fixture
        .realm
        .workload_token(&client_id(PROJECT, "senzory"), json!(SLUG));
    let (status, body) = read(&fixture, SLUG, &member).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

/// F-SEC1-009: while the gateway holds no Organization for the Endpoint's project (one
/// Organization per installation), a person whose token names no group at all is still no member
/// of it: the empty project is refused, a person of any project is admitted.
#[tokio::test]
async fn a_person_of_no_group_is_not_admitted_while_the_organization_is_unknown() {
    let fixture = fixture_of("no-group-unknown-organization", false).await;
    let nobody = person(&fixture.realm, SLUG, &[]);
    let (status, body) = read(&fixture, SLUG, &nobody).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(fixture.broker.hops().len(), 0, "the broker was never asked");
    let member = person(&fixture.realm, SLUG, &[SIBLING]);
    let (status, body) = read(&fixture, SLUG, &member).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}
