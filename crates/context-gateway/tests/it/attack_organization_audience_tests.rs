//! Attack vector: a person or workload of one Organization calls another Organization's
//! `audience: organization` Endpoint (T-5557; EP-14, ADR-N-058 2.4, Architecture/04 §3).
//!
//! One installation serves several Organizations from one realm, each with its own group subtree
//! (ADR-N-058). An `organization` audience admits the members of the Endpoint's own Organization:
//! a person whose verified `jc_org` names it (ADR-N-067 2.3), a person whose groups name one of
//! its projects, or a ServiceAccount of one of them. A caller of
//! another Organization holds a valid token of the same realm, so the token alone says nothing;
//! the gateway reads each project's Organization from the Project manifests (`Provenance`).
//!
//! The outsider is answered exactly as a slug nobody created is, so the answer does not tell an
//! Endpoint of another Organization apart from one that does not exist (PF-32, R20).

use crate::common;

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
        page: None,
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

/// F-SEC1-009 under ADR-N-067: a person whose groups name no project at all and whose token
/// carries no `jc_org` (a `platform-readers`-only token) is no member of any Organization.
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
    // A project group is no `jc_org`: the broker grants them no `organization` assignee.
    for hop in fixture.broker.hops() {
        assert!(
            hop.headers.get("x-jc-member").is_none(),
            "{:?}",
            hop.headers
        );
    }
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

/// A person by the claims ADR-N-067 2.2 adds: `jc_org` and `jc_groups` beside full-path
/// `groups`, with `roles` as their realm roles.
fn member(
    realm: &common::Realm,
    slug: &str,
    organization: &str,
    groups: &[&str],
    roles: &[&str],
) -> String {
    let paths: Vec<String> = groups
        .iter()
        .map(|group| format!("/{organization}/{group}"))
        .collect();
    realm.mint(&json!({
        "iss": common::ISSUER,
        "sub": "f:1:member",
        "aud": slug,
        "exp": common::in_seconds(300),
        "iat": common::in_seconds(-10),
        "preferred_username": "demo.orgmember",
        "groups": paths,
        "jc_org": [organization],
        "jc_groups": paths,
        "realm_access": { "roles": roles },
    }))
}

/// ADR-N-067 2.3, EP-171: a member of the Endpoint's Organization, in no project, is admitted by `jc_org`
/// and reads what the Policy grants them.
#[tokio::test]
async fn a_member_of_the_organization_in_no_project_is_admitted_by_jc_org() {
    let fixture = fixture("member-by-jc-org").await;
    let token = member(&fixture.realm, SLUG, "hel", &["members"], &["member"]);
    let (status, body) = read(&fixture, SLUG, &token).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        fixture.broker.hops().len(),
        1,
        "the read reached the broker"
    );
}

/// A person bound by RoleBinding alone (T-6461): the token names the project's group in `groups`
/// and `jc_groups`, with the path of `hel` when `jc_org` names nobody; `jc_org` comes from the
/// Organization's own groups, never from this one (T-6470).
fn bound(realm: &common::Realm, slug: &str, jc_org: Option<&str>, group: &str) -> String {
    let path = format!("/{}/{group}", jc_org.unwrap_or("hel"));
    realm.mint(&json!({
        "iss": common::ISSUER,
        "sub": "f:1:bound",
        "aud": slug,
        "exp": common::in_seconds(300),
        "iat": common::in_seconds(-10),
        "preferred_username": "t6025-full",
        "groups": ["platform-readers", group],
        "jc_org": jc_org.into_iter().collect::<Vec<_>>(),
        "jc_groups": [path],
        "realm_access": { "roles": ["member"] },
    }))
}

/// EP-172, T-6461, T-6451 turns 04-05: the org-admin who made an Endpoint reads it the moment their
/// binding fills the project's group; the same group of another Organization's project does not,
/// and it is no project of the Endpoint's, so nothing is learned about the slug.
#[tokio::test]
async fn a_person_bound_in_a_project_is_admitted_to_its_organization_endpoint() {
    let fixture = fixture("bound-organization").await;
    let group = format!("{PROJECT}-readers");
    let token = bound(&fixture.realm, SLUG, Some("hel"), &group);
    let (status, body) = read(&fixture, SLUG, &token).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let hops = fixture.broker.hops().len();
    let outsider = bound(&fixture.realm, SLUG, Some("zilina"), "zilina-readers");
    let (status, refused) = read(&fixture, SLUG, &outsider).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{refused}");
    assert_eq!(
        fixture.broker.hops().len(),
        hops,
        "the broker was not asked"
    );
}

/// EP-173, T-6470: a vendor bound on one project holds its group and no `jc_org`. They read that
/// project's `organization` Endpoint; on another project's Endpoint of the same Organization they
/// are the outsider of EP-14, answered as a slug nobody made, and the broker is not asked. The
/// `jc_groups` path of the project's group proves nothing about the Organization.
#[tokio::test]
async fn a_vendor_bound_on_a_project_reaches_only_that_projects_organization_endpoint() {
    let fixture = fixture("bound-vendor").await;
    let own = bound(&fixture.realm, SLUG, None, &format!("{PROJECT}-writers"));
    let (status, body) = read(&fixture, SLUG, &own).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let hops = fixture.broker.hops().len();
    let sibling = format!("{SIBLING}-readers");
    let vendor = bound(&fixture.realm, SLUG, None, &sibling);
    let (status, refused) = read(&fixture, SLUG, &vendor).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{refused}");
    let nobodys = bound(&fixture.realm, NOBODYS, None, &sibling);
    let (_, unknown) = read(&fixture, NOBODYS, &nobodys).await;
    assert_eq!(refused, unknown, "told apart from a slug nobody made");
    assert_eq!(
        fixture.broker.hops().len(),
        hops,
        "the broker was not asked"
    );

    // The same person of the Organization, whose `/hel/members` gives `jc_org`, keeps the reach.
    let member = bound(&fixture.realm, SLUG, Some("hel"), &sibling);
    let (status, body) = read(&fixture, SLUG, &member).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

/// EP-172, T-6461: `{project}-readers` and `-writers` name the project on a `project-list` audience as a
/// group of the project's name does; a sibling's group, or the project's name with another
/// ending, stays outside (EP-14, PF-87).
#[tokio::test]
async fn a_person_bound_in_the_project_is_admitted_to_its_project_list_endpoint() {
    let realm = common::Realm::new();
    let broker = common::BrokerStub::start(vec![json!([]), json!([])]).await;
    let repo = Repo::new("bound-project-list", &[PROJECT, OUTSIDER]);
    let accounts =
        accounts_of(&jc_manifest::loader::Repository::load(&repo.0).expect("the repository loads"));
    let listed = Endpoint {
        audience: Audience::ProjectList,
        ..endpoint()
    };
    let gateway = Gateway::new(
        Broker::new(broker.url.clone()),
        Box::new(PolicyPdp),
        "hel.fi",
    )
    .serve([listed])
    .authenticate(Arc::new(realm.verifier()), accounts, None);
    let fixture = Fixture {
        realm,
        app: router(Arc::new(gateway)),
        broker,
        _repo: repo,
    };
    for ending in ["readers", "writers"] {
        let token = bound(
            &fixture.realm,
            SLUG,
            Some("hel"),
            &format!("{PROJECT}-{ending}"),
        );
        let (status, body) = read(&fixture, SLUG, &token).await;
        assert_eq!(status, StatusCode::OK, "{ending}: {body}");
    }
    let hops = fixture.broker.hops().len();
    for group in [format!("{SIBLING}-readers"), format!("{PROJECT}-auditors")] {
        let token = bound(&fixture.realm, SLUG, Some("hel"), &group);
        let (status, body) = read(&fixture, SLUG, &token).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{group}: {body}");
    }
    assert_eq!(
        fixture.broker.hops().len(),
        hops,
        "the broker was not asked"
    );
}

/// ADR-N-067 2.3, EP-171: admission is only the door. The broker decides the read (ADR-N-050)
/// and hears the member as they are: their groups in the Organization and their own roles, no
/// project and no role the door added, so a member the Policies grant nothing reads nothing.
#[tokio::test]
async fn a_member_without_a_grant_reaches_the_broker_with_nothing_added() {
    let fixture = fixture("member-without-grant").await;
    let token = member(&fixture.realm, SLUG, "hel", &["members"], &[]);
    let (status, body) = read(&fixture, SLUG, &token).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let hops = fixture.broker.hops();
    let [hop] = hops.as_slice() else {
        panic!("one hop, got {}", hops.len())
    };
    let heard = |name: &str| {
        hop.headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned()
    };
    assert_eq!(heard("x-jc-groups"), "members");
    // The broker grants an `organization` assignee on this verified marker (Antares T-6419).
    assert_eq!(heard("x-jc-member"), "true");
    assert!(
        !heard("x-jc-roles").contains("member"),
        "{}",
        heard("x-jc-roles")
    );
    assert!(!heard("x-jc-groups").contains(PROJECT));
}

/// ADR-N-067 §3, EP-171: a member of Žilina, with groups named like Helsinki's projects and `members`,
/// reaches nothing of Helsinki's Endpoint and is told what a missing slug tells.
#[tokio::test]
async fn a_member_of_another_organization_with_same_named_groups_reads_nothing() {
    let fixture = fixture("member-of-another").await;
    let groups = ["members", PROJECT, SIBLING];
    let token = member(&fixture.realm, SLUG, OUTSIDER, &groups, &["member"]);
    let (status, body) = read(&fixture, SLUG, &token).await;
    let nobodys = member(&fixture.realm, NOBODYS, OUTSIDER, &groups, &["member"]);
    let (missing_status, missing_body) = read(&fixture, NOBODYS, &nobodys).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!((status, body), (missing_status, missing_body));
    assert_eq!(fixture.broker.hops().len(), 0, "the broker was never asked");
}

/// ADR-N-067 2.2: `jc_org` is not a header a client sends. A person without the claim who sends
/// one is still no member.
#[tokio::test]
async fn a_jc_org_header_admits_nobody() {
    let fixture = fixture("jc-org-header").await;
    let token = person(&fixture.realm, SLUG, &["platform-readers"]);
    let response = fixture
        .app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/api/endpoint/{SLUG}/ngsi-ld/v1/entities?type=BikeHireDockingStation"
                ))
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header("jc_org", "hel")
                .header("x-jc-org", "hel")
                .body(Body::empty())
                .expect("a request"),
        )
        .await
        .expect("the gateway answers");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(fixture.broker.hops().len(), 0, "the broker was never asked");
}

/// ADR-N-067 §3: an anonymous read of an organization Endpoint carries no entity body and never
/// reaches the broker. It stays the 401 that tells a client to sign in (EP-16, GW22).
#[tokio::test]
async fn an_anonymous_read_of_an_organization_endpoint_returns_no_entity() {
    let fixture = fixture("anonymous").await;
    let response = fixture
        .app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/api/endpoint/{SLUG}/ngsi-ld/v1/entities?type=BikeHireDockingStation"
                ))
                .body(Body::empty())
                .expect("a request"),
        )
        .await
        .expect("the gateway answers");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("a readable body");
    let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    assert!(!body.is_array() && body.get("id").is_none(), "{body}");
    assert_eq!(fixture.broker.hops().len(), 0, "the broker was never asked");
}
