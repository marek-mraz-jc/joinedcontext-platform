//! Edge cases of `app::subject_of` (T-2493, PF-35, PF-46, EP-14, EP-15, EP-16).
//!
//! **The contract.** Verified claims become exactly one kind of subject: a data space consumer
//! (T-1896's, not repeated), a workload found by `azp` among the ServiceAccount manifests, or a
//! person matched by `preferred_username` and a group naming a project. Each path checks
//! `endpoint.admits(project)` before it hands out grants.
//!
//! **Inputs.** The claims (`azp`, `groups`, `preferred_username`, `realm_access.roles`,
//! `agreementId`) and the endpoint (project, allowed projects, audience).
//!
//! The subject is read back from `GET …/access`, which names it and lists the grants that apply
//! to it, so a role or a group that reaches the PDP shows up as a grant.
//! Struck: "two allowed groups resolve to the first match" cannot be observed, because the
//! project found is used for `admits` only and both matches admit.

mod common;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use context_gateway::app::{router, Gateway};
use context_gateway::auth::accounts::{accounts_of, client_id};
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::Endpoint;
use jc_core::kinds::{Audience, PolicySpec, Representation};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;
use tower::ServiceExt;

const SLUG: &str = "k4y7pq2mzt6vhx3nbwrs5cjd8f";
const PROJECT: &str = "ovzdusie";

/// A grant of `queryEntity` on `marker` to one assignee: which markers a caller's access
/// document lists says which of these reached them.
fn policy(assignee: &str, id: &str, marker: &str) -> PolicySpec {
    serde_norway::from_str(&format!(
        "contextSpaceRef: ovzdusie\nassigner: did:web:banskabystrica.sk\n\
         assignee: {{ kind: {assignee}, id: {id} }}\noperations: [queryEntity]\n\
         information:\n  - entities:\n      - type: {marker}\n"
    ))
    .expect("the policy spec parses")
}

fn endpoint(audience: Audience) -> Endpoint {
    Endpoint {
        declared_types: None,
        roles: Default::default(),
        slug: SLUG.to_owned(),
        title: Default::default(),
        description: Default::default(),
        space: "ovzdusie".to_owned(),
        project: PROJECT.to_owned(),
        audience,
        allowed_projects: vec!["doprava".to_owned()],
        representations: vec![Representation::NgsiLd],
        rate_limit: None,
        file_limits: None,
        hidden_attributes: Default::default(),
        projection: None,
        base_path: format!("/api/endpoint/{SLUG}"),
        models: Vec::new(),
        view_mapping: None,
        catalog: None,
        policies: vec![
            policy("role", "data-steward", "StewardView"),
            policy("role", "sensor-writer", "SensorWrite"),
            policy("serviceAccount", "senzory", "AccountOwn"),
            policy("group", "stewards", "GroupView"),
            policy("role", "public", "PublicView"),
        ],
    }
}

/// A repository holding the ServiceAccount `senzory` of `project`, with `sensor-writer`.
struct Repo(PathBuf);

impl Repo {
    fn new(name: &str, project: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("gateway-subject-edge-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        let space = dir.join(format!("projects/{project}/spaces/{project}"));
        std::fs::create_dir_all(&space).expect("a space directory");
        std::fs::write(
            space.join("space.yaml"),
            format!("apiVersion: joinedcontext.com/v1alpha1\nkind: ContextSpace\nmetadata:\n  name: {project}\n  namespace: {project}\nspec:\n  urnSegment: {project}\n"),
        )
        .expect("the space");
        let accounts = dir.join(format!("projects/{project}/access/serviceaccounts"));
        std::fs::create_dir_all(&accounts).expect("an account directory");
        std::fs::write(
            accounts.join("senzory.yaml"),
            format!("apiVersion: joinedcontext.com/v1alpha1\nkind: ServiceAccount\nmetadata:\n  name: senzory\n  namespace: {project}\nspec:\n  owner:\n    user: demo.steward\n  purpose: \"edge cases\"\n  roles:\n    - role: sensor-writer\n      scope: {{ project: {project} }}\n  credentials:\n    - kind: oauth-client\n      name: default\n"),
        )
        .expect("the account");
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
    _repo: Repo,
}

fn fixture(name: &str, audience: Audience, account_project: &str) -> Fixture {
    let realm = common::Realm::new();
    let repo = Repo::new(name, account_project);
    let accounts =
        accounts_of(&jcctl::loader::Repository::load(&repo.0).expect("the repository loads"));
    let app = router(Arc::new(
        Gateway::new(
            Broker::new("http://127.0.0.1:1"),
            Box::new(PolicyPdp),
            "banskabystrica.sk",
        )
        .serve([endpoint(audience)])
        .authenticate(Arc::new(realm.verifier()), accounts, None),
    ));
    Fixture {
        realm,
        app,
        _repo: repo,
    }
}

/// The claims a token carries beside the ones every token has.
fn token(realm: &common::Realm, extra: Value) -> String {
    let mut claims = json!({
        "iss": common::ISSUER,
        "sub": "f:1:someone",
        "aud": SLUG,
        "exp": common::in_seconds(300),
        "iat": common::in_seconds(-10),
    });
    for (key, value) in extra.as_object().expect("an object") {
        claims[key] = value.clone();
    }
    realm.mint(&claims)
}

/// `GET …/access` with the token: the status and the document.
async fn access(fixture: &Fixture, extra: Value) -> (StatusCode, Value) {
    let response = fixture
        .app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/api/endpoint/{SLUG}/access"))
                .header(
                    header::AUTHORIZATION,
                    format!("Bearer {}", token(&fixture.realm, extra)),
                )
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

/// The markers of the grants that reached the subject, sorted.
fn granted(document: &Value) -> Vec<String> {
    let mut out: Vec<String> = document["permissions"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|entry| Some(entry["resource"]["type"].as_str()?.to_owned()))
        .collect();
    out.sort();
    out
}

fn workload() -> String {
    client_id(PROJECT, "senzory")
}

/// PF-46: a client no ServiceAccount manifest names, with no person behind it, is refused, and
/// never read as the anonymous public caller, not even on a public endpoint.
#[tokio::test]
async fn an_azp_naming_no_service_account_and_no_username_is_unauthorized_not_public() {
    for audience in [Audience::Public, Audience::Organization] {
        let fixture = fixture("unknown-azp", audience, PROJECT);
        let (status, document) = access(&fixture, json!({ "azp": "some-other-client" })).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{audience:?}: {document}");
        assert!(granted(&document).is_empty(), "{document}");
    }
}

/// PF-35: a workload of a project the endpoint does not serve gets nothing here.
#[tokio::test]
async fn a_service_account_whose_project_the_endpoint_does_not_admit_is_forbidden() {
    let fixture = fixture("foreign-account", Audience::ProjectList, "zdravie");
    let (status, document) =
        access(&fixture, json!({ "azp": client_id("zdravie", "senzory") })).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{document}");
}

/// PF-35: a workload is its manifest: the person and the realm roles a token also carries add
/// nothing to what the account's own bindings grant.
#[tokio::test]
async fn an_azp_naming_a_real_service_account_ignores_any_human_claims_in_the_same_token() {
    let fixture = fixture("account-with-human-claims", Audience::Organization, PROJECT);
    let (status, document) = access(
        &fixture,
        json!({
            "azp": workload(),
            "preferred_username": "jana.novakova",
            // A group that names a project only; a group Policy never reaches a workload
            // (T-2545, `a_service_accounts_token_groups_reach_no_group_policy`).
            "groups": ["/ovzdusie"],
            "realm_access": { "roles": ["data-steward"] },
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{document}");
    assert_eq!(
        document["subject"],
        json!({ "type": "serviceAccount", "id": "senzory" })
    );
    assert_eq!(
        granted(&document),
        ["AccountOwn", "SensorWrite"],
        "{document}"
    );
}

/// PF-35, T-2545: a workload's grants are its manifest's. A `groups` claim on its token — a
/// Keycloak admin putting the service-account user into a group — reaches no Policy granted to
/// that group, because that membership is not a reviewed change.
#[tokio::test]
async fn a_service_accounts_token_groups_reach_no_group_policy() {
    let fixture = fixture("account-in-a-group", Audience::Organization, PROJECT);
    let (status, document) = access(
        &fixture,
        json!({ "azp": workload(), "groups": ["/stewards"] }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{document}");
    assert_eq!(
        granted(&document),
        ["AccountOwn", "SensorWrite"],
        "{document}"
    );
}

/// PF-46: when both are present, a known `azp` decides; the person's name is not the subject.
#[tokio::test]
async fn a_token_with_azp_and_preferred_username_prefers_the_service_account_path() {
    let fixture = fixture("both-names", Audience::Organization, PROJECT);
    let (_, document) = access(
        &fixture,
        json!({ "azp": workload(), "preferred_username": "jana.novakova" }),
    )
    .await;
    assert_eq!(document["subject"]["type"], "serviceAccount", "{document}");

    // An `azp` no manifest names, with a person: the person, as a browser login is.
    let (status, document) = access(
        &fixture,
        json!({ "azp": "portal", "preferred_username": "jana.novakova", "groups": ["ovzdusie"] }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{document}");
    assert_eq!(
        document["subject"],
        json!({ "type": "user", "id": "jana.novakova" })
    );
}

/// DS-01: a token that names an agreement is a data space consumer's or nothing: without the
/// agreement it is refused, never read as the workload or the person it also names.
#[tokio::test]
async fn a_dataspace_token_never_falls_through_to_the_workload_or_human_branch() {
    let fixture = fixture("dataspace", Audience::Public, PROJECT);
    let (status, document) = access(
        &fixture,
        json!({
            "agreementId": "urn:uuid:no-such-agreement",
            "azp": workload(),
            "preferred_username": "jana.novakova",
            "groups": ["ovzdusie"],
            "realm_access": { "roles": ["data-steward"] },
        }),
    )
    .await;
    assert!(status.is_client_error(), "{status}: {document}");
    assert_ne!(document["subject"]["type"], "serviceAccount", "{document}");
    assert_ne!(document["subject"]["type"], "user", "{document}");
}

/// EP-14, EP-16: a person in none of the endpoint's projects is refused on a project list and
/// admitted with the public role on a public endpoint.
#[tokio::test]
async fn a_human_in_none_of_the_endpoints_allowed_groups_gets_the_empty_project_and_is_admitted_only_if_public(
) {
    let outsider = json!({ "preferred_username": "eva", "groups": ["/zdravie"] });
    let list = fixture("outsider-list", Audience::ProjectList, PROJECT);
    let (status, _) = access(&list, outsider.clone()).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let public = fixture("outsider-public", Audience::Public, PROJECT);
    let (status, document) = access(&public, outsider).await;
    assert_eq!(status, StatusCode::OK, "{document}");
    assert_eq!(granted(&document), ["PublicView"], "{document}");
}

/// EP-15: Keycloak writes a group as its path; `/doprava` is the allowed project `doprava`.
#[tokio::test]
async fn a_group_with_a_leading_slash_is_trimmed_before_matching() {
    let fixture = fixture("slash", Audience::ProjectList, PROJECT);
    for group in ["/doprava", "doprava", "/ovzdusie"] {
        let (status, document) = access(
            &fixture,
            json!({ "preferred_username": "jana", "groups": [group] }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{group}: {document}");
    }
    // Only the leading slash: a subgroup path is another group.
    let (status, _) = access(
        &fixture,
        json!({ "preferred_username": "jana", "groups": ["/city/ovzdusie", "ovzdusie/"] }),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

/// PF-46: no roles claim is no roles, never a default grant; on a public endpoint a login keeps
/// the public role and nothing more.
#[tokio::test]
async fn no_roles_claim_yields_an_empty_role_set_rather_than_a_default_grant() {
    let organization = fixture("no-roles", Audience::Organization, PROJECT);
    let (status, document) = access(&organization, json!({ "preferred_username": "jana" })).await;
    assert_eq!(status, StatusCode::OK, "{document}");
    assert!(granted(&document).is_empty(), "{document}");

    let public = fixture("no-roles-public", Audience::Public, PROJECT);
    let (_, document) = access(&public, json!({ "preferred_username": "jana" })).await;
    assert_eq!(granted(&document), ["PublicView"], "{document}");
}

/// EP-15: `groups: []` and no `groups` claim are the same person with no project.
#[tokio::test]
async fn an_empty_groups_list_is_the_same_as_no_groups_claim() {
    let fixture = fixture("empty-groups", Audience::ProjectList, PROJECT);
    let (without, _) = access(&fixture, json!({ "preferred_username": "jana" })).await;
    let (empty, _) = access(
        &fixture,
        json!({ "preferred_username": "jana", "groups": [] }),
    )
    .await;
    assert_eq!(without, StatusCode::FORBIDDEN);
    assert_eq!(empty, without);
}

/// PF-46: an account is found by its Keycloak client id, never by its bare manifest name, and a
/// token of another realm never reaches the lookup.
#[tokio::test]
async fn a_service_account_resolved_for_the_wrong_realm_is_never_looked_up_by_name_alone() {
    let fixture = fixture("bare-name", Audience::Organization, PROJECT);
    let (status, document) = access(&fixture, json!({ "azp": "senzory" })).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{document}");

    let (status, _) = access(
        &fixture,
        json!({ "azp": workload(), "iss": "https://evil.example/realms/joinedcontext" }),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let other_realm = common::Realm::new();
    let forged = token(&other_realm, json!({ "azp": workload() }));
    let response = fixture
        .app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/api/endpoint/{SLUG}/access"))
                .header(header::AUTHORIZATION, format!("Bearer {forged}"))
                .body(Body::empty())
                .expect("a request"),
        )
        .await
        .expect("the gateway answers");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}
