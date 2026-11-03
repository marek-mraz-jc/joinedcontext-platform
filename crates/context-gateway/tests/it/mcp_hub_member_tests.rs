//! The MCP hub for a member of an Organization in no project (ADR-N-067 2.4, step 5): `search`,
//! `list_endpoints` and the `catalog` Endpoint answer an anonymous caller with the public
//! Endpoints, and a signed-in member with the public ones plus every Endpoint a Policy lets
//! them read. The hub token's `endpoint:*` scope picks every Endpoint (EP-88); the grants decide.
//!
//! Three callers, three lists: anonymous, `demo.orgmember` (in `/hel/members`) and
//! `demo.analyst` (also in `/hel/traffic-analysts`). No answer names, describes or counts an
//! Endpoint the caller may not read.

use crate::common;

use axum::body::Body;
use axum::http::{Method, Request};
use axum::Router;
use context_gateway::app::{router, Gateway};
use context_gateway::auth::accounts::ServiceAccounts;
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::{Endpoint, Model};
use context_gateway::store::Provenance;
use jc_core::kinds::{Audience, PolicySpec, Representation};
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

/// Public: everyone reads it.
const OPEN: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaa";
/// Shared with the whole Organization `hel`.
const SHARED: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbb";
/// Shared with `/hel/traffic-analysts` only.
const TEAM: &str = "cccccccccccccccccccccccccc";
/// Of `hel`, granted to a role no member holds.
const CLOSED: &str = "dddddddddddddddddddddddddd";
/// Shared with the whole Organization `zilina`.
const OTHER: &str = "eeeeeeeeeeeeeeeeeeeeeeeeee";

/// The words only the Endpoints nobody here may read carry.
const SECRET_WORDS: &[&str] = &["Closedbox", "Zilinabox", CLOSED, OTHER];

fn grant(space: &str, assignee: &str) -> PolicySpec {
    serde_norway::from_str(&format!(
        "contextSpaceRef: {space}\n\
         assigner: did:web:hel.fi\n\
         assignee: {assignee}\n\
         operations: [queryEntity]\n\
         information:\n  \
         - entities:\n      \
         - type: TrafficFlowObserved\n"
    ))
    .expect("the policy spec parses")
}

fn endpoint(
    slug: &str,
    title: &str,
    project: &str,
    audience: Audience,
    assignee: &str,
) -> Endpoint {
    let space = format!("space-{}", &slug[..1]);
    Endpoint {
        declared_types: None,
        roles: Default::default(),
        slug: slug.to_owned(),
        title: [("en".to_owned(), title.to_owned())].into_iter().collect(),
        description: [("en".to_owned(), format!("{title} traffic counts"))]
            .into_iter()
            .collect(),
        space: space.clone(),
        project: project.to_owned(),
        audience,
        allowed_projects: Vec::new(),
        representations: vec![Representation::NgsiLd, Representation::Mcp],
        rate_limit: None,
        creates: None,
        file_limits: None,
        hidden_attributes: Default::default(),
        projection: None,
        view_mapping: None,
        catalog: None,
        deprecation: None,
        feeds: Vec::new(),
        calendars: Vec::new(),
        map_defaults: Default::default(),
        page: None,
        base_path: format!("/api/endpoint/{slug}"),
        models: vec![Model {
            name: "traffic".to_owned(),
            version: "1.0.0".to_owned(),
            major: 1,
            classes: vec!["TrafficFlowObserved".to_owned()],
            json_schema: None,
            context: None,
        }],
        policy_names: Vec::new(),
        manifest_name: String::new(),
        policies: vec![grant(&space, assignee)],
    }
}

fn app(realm: &common::Realm) -> Router {
    let organization = Audience::Organization;
    let gateway = Gateway::new(
        Broker::new("http://127.0.0.1:1"),
        Box::new(PolicyPdp),
        "hel.fi",
    )
    .serve([
        endpoint(
            OPEN,
            "Openbox",
            "mobility",
            Audience::Public,
            "{ kind: role, id: public }",
        ),
        endpoint(
            SHARED,
            "Sharedbox",
            "mobility",
            organization,
            "{ kind: organization }",
        ),
        endpoint(
            TEAM,
            "Teambox",
            "mobility",
            organization,
            "{ kind: group, id: traffic-analysts }",
        ),
        endpoint(
            CLOSED,
            "Closedbox",
            "mobility",
            organization,
            "{ kind: role, id: auditor }",
        ),
        endpoint(
            OTHER,
            "Zilinabox",
            "zilina",
            organization,
            "{ kind: organization }",
        ),
    ])
    .authenticate(Arc::new(realm.verifier()), ServiceAccounts::new(), None);
    gateway.replace_provenance(Provenance {
        organizations: [("mobility", "hel"), ("zilina", "zilina")]
            .into_iter()
            .map(|(project, organization)| (project.to_owned(), organization.to_owned()))
            .collect(),
        ..Default::default()
    });
    router(Arc::new(gateway))
}

/// A hub token of a person of `organization` in `groups` there, in no project, as Keycloak
/// mints it for the `mcp-hub` client (ADR-N-067 2.2, EP-88).
fn member(realm: &common::Realm, organization: &str, groups: &[&str]) -> String {
    let paths: Vec<String> = groups
        .iter()
        .map(|group| format!("/{organization}/{group}"))
        .collect();
    realm.mint(&json!({
        "iss": common::ISSUER,
        "sub": "f:1:member",
        "aud": ["mcp-hub"],
        "azp": "mcp-hub",
        "scope": "openid endpoint:*",
        "preferred_username": "demo.orgmember",
        "groups": paths,
        "jc_org": [organization],
        "jc_groups": paths,
        "realm_access": { "roles": ["default-roles-dev"] },
        "exp": common::in_seconds(300),
        "iat": common::in_seconds(-10),
    }))
}

fn call(name: &str, arguments: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": { "name": name, "arguments": arguments },
    })
}

/// The whole answer of one hub message, as text and parsed.
async fn send(app: &Router, token: Option<&str>, body: Value) -> (String, Value) {
    let mut builder = Request::builder()
        .method(Method::POST)
        .uri("/api/mcp")
        .header("content-type", "application/json");
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    let response = app
        .clone()
        .oneshot(
            builder
                .body(Body::from(body.to_string()))
                .expect("a request"),
        )
        .await
        .expect("the gateway answers");
    let bytes = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024)
        .await
        .expect("a body");
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let parsed = serde_json::from_str(&text).unwrap_or(Value::Null);
    (text, parsed)
}

/// Every slug `list_endpoints` answers, one page of one row at a time to the last `nextCursor`
/// (KNOWLEDGE T-6061), every page's text, and the first page's `total`.
async fn walked(app: &Router, token: Option<&str>) -> (Vec<String>, String, Value) {
    let mut slugs = Vec::new();
    let mut pages = String::new();
    let mut total = Value::Null;
    let mut arguments = json!({ "limit": 1 });
    for _ in 0..10 {
        let (text, answer) = send(app, token, call("list_endpoints", arguments.clone())).await;
        pages.push_str(&text);
        let page = &answer["result"]["structuredContent"];
        if total.is_null() {
            total = page["total"].clone();
        }
        slugs.extend(
            page["endpoints"]
                .as_array()
                .unwrap_or_else(|| panic!("no list: {answer}"))
                .iter()
                .map(|entry| entry["slug"].as_str().expect("a slug").to_owned()),
        );
        match page.get("nextCursor") {
            Some(cursor) => arguments = json!({ "limit": 1, "cursor": cursor }),
            None => return (slugs, pages, total),
        }
    }
    panic!("the cursor never ended: {pages}");
}

/// The identifiers of the catalogue's `DataService` entities, one per Endpoint, and its text.
async fn catalogued(app: &Router, token: Option<&str>) -> (Vec<String>, String) {
    let (text, answer) = send(
        app,
        token,
        call(
            "query_entities",
            json!({ "endpoint": "catalog", "type": "DataService", "format": "keyValues", "limit": 100 }),
        ),
    )
    .await;
    let entities = answer["result"]["structuredContent"]["entities"]
        .as_array()
        .unwrap_or_else(|| panic!("no entities: {answer}"));
    let mut slugs: Vec<String> = entities
        .iter()
        .map(|entity| entity["identifier"].as_str().unwrap_or_default().to_owned())
        .collect();
    slugs.sort();
    (slugs, text)
}

/// ADR-N-067 2.4, EP-88: anonymous, a member and an analyst each list a different set, the same
/// in `list_endpoints`, the `catalog` Endpoint and `search`; nothing names what they cannot read.
#[tokio::test]
async fn anonymous_a_member_and_an_analyst_each_see_their_own_endpoints() {
    let realm = common::Realm::new();
    let app = app(&realm);
    let orgmember = member(&realm, "hel", &["members"]);
    let analyst = member(&realm, "hel", &["members", "traffic-analysts"]);

    for (who, token, expected) in [
        ("anonymous", None, vec![OPEN]),
        (
            "demo.orgmember",
            Some(orgmember.as_str()),
            vec![OPEN, SHARED],
        ),
        (
            "demo.analyst",
            Some(analyst.as_str()),
            vec![OPEN, SHARED, TEAM],
        ),
    ] {
        let (slugs, pages, total) = walked(&app, token).await;
        assert_eq!(slugs, expected, "{who}: {pages}");
        assert_eq!(
            total,
            json!(expected.len()),
            "{who}: the count is of readable ones"
        );

        let (catalogue, catalogue_text) = catalogued(&app, token).await;
        assert_eq!(catalogue, expected, "{who}: {catalogue_text}");

        let (search_text, search) = send(
            &app,
            token,
            call("search", json!({ "query": "traffic counts box" })),
        )
        .await;
        assert_eq!(
            search["result"]["isError"],
            json!(false),
            "{who}: {search_text}"
        );
        for slug in [OPEN, SHARED, TEAM] {
            assert_eq!(
                search_text.contains(slug),
                expected.contains(&slug),
                "{who} and {slug}: {search_text}"
            );
        }

        for text in [&pages, &catalogue_text, &search_text] {
            for word in SECRET_WORDS {
                assert!(!text.contains(word), "{who} was told {word}: {text}");
            }
        }
    }
}

/// ADR-N-067 2.3: a member of another Organization, with groups named like `hel`'s, reaches
/// none of `hel`'s internal Endpoints and sees their own Organization's.
#[tokio::test]
async fn a_member_of_another_organization_sees_none_of_hels_internal_endpoints() {
    let realm = common::Realm::new();
    let app = app(&realm);
    let outsider = member(&realm, "zilina", &["members", "traffic-analysts"]);
    let (slugs, pages, _) = walked(&app, Some(&outsider)).await;
    assert_eq!(slugs, [OPEN, OTHER], "{pages}");
    for word in ["Sharedbox", "Teambox", "Closedbox", SHARED, TEAM, CLOSED] {
        assert!(!pages.contains(word), "{word}: {pages}");
    }
}

/// EP-173, T-6470: a vendor bound on `mobility` by RoleBinding holds its group and no `jc_org`.
/// The hub lists what is public; the Endpoint shared with the whole Organization is not for them,
/// though it lives in the very project they are bound on.
#[tokio::test]
async fn a_vendor_bound_on_a_project_matches_no_grant_to_the_organization() {
    let realm = common::Realm::new();
    let app = app(&realm);
    let vendor = realm.mint(&json!({
        "iss": common::ISSUER,
        "sub": "f:1:vendor",
        "aud": ["mcp-hub"],
        "azp": "mcp-hub",
        "scope": "openid endpoint:*",
        "preferred_username": "vendor",
        "groups": ["mobility-readers"],
        "jc_groups": ["/hel/mobility-readers"],
        "realm_access": { "roles": ["default-roles-dev"] },
        "exp": common::in_seconds(300),
        "iat": common::in_seconds(-10),
    }));
    let (slugs, pages, _) = walked(&app, Some(&vendor)).await;
    assert_eq!(slugs, [OPEN], "{pages}");
    for word in ["Sharedbox", SHARED] {
        assert!(!pages.contains(word), "{word}: {pages}");
    }
}

/// The hub keeps a caller's reach for a few seconds by what its token says (T-4528). A token
/// whose Organization claims differ, all else equal, is another caller: a reach kept for a
/// member never answers the same person once `jc_org` and `jc_groups` no longer name `hel`.
#[tokio::test]
async fn a_kept_reach_follows_the_organization_claims() {
    let realm = common::Realm::new();
    let app = app(&realm);
    let analyst = member(&realm, "hel", &["traffic-analysts"]);
    let (slugs, _, _) = walked(&app, Some(&analyst)).await;
    assert_eq!(slugs, [OPEN, SHARED, TEAM]);

    // The same subject, groups and roles; only the Organization claims are gone.
    let left = realm.mint(&json!({
        "iss": common::ISSUER,
        "sub": "f:1:member",
        "aud": ["mcp-hub"],
        "azp": "mcp-hub",
        "scope": "openid endpoint:*",
        "preferred_username": "demo.orgmember",
        "groups": ["/hel/traffic-analysts"],
        "realm_access": { "roles": ["default-roles-dev"] },
        "exp": common::in_seconds(300),
        "iat": common::in_seconds(-10),
    }));
    let (slugs, pages, _) = walked(&app, Some(&left)).await;
    assert_eq!(slugs, [OPEN], "{pages}");
}
