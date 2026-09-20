//! Edge cases of `app::space_catalog`, `GET /cs` (T-1885, EP-26, MP-02).
//!
//! Contract, in one sentence: the catalog lists exactly the spaces this caller could open one by
//! one — each authenticated against that space's own endpoint and decided by the same PDP that
//! enforces a request — and a space it leaves out leaves no trace in the document: no name, no
//! title, no count and no refusal (SP-11, R20, T-0814).

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use context_gateway::app::{router, Gateway};
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::{Endpoint, Space};
use jc_core::kinds::{Audience, PolicySpec, Representation};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::Arc;
use tower::ServiceExt;

const OPEN: &str = "ovzdusie";
const BOOKS: &str = "uctovnictvo";
const PAYROLL: &str = "mzdy";
const HOST: &str = "https://bb.example.sk";

fn policy(yaml: &str) -> PolicySpec {
    serde_norway::from_str(yaml).expect("the policy spec parses")
}

fn granted_to(space: &str, role: &str) -> PolicySpec {
    policy(&format!(
        r#"contextSpaceRef: {space}
assigner: did:web:banskabystrica.sk
assignee: {{ kind: role, id: {role} }}
operations: [queryEntity, retrieveEntity]
"#
    ))
}

fn space_named(space: &str, role: &str) -> Space {
    Space {
        endpoint: Arc::new(Endpoint {
            slug: space.to_owned(),
            title: BTreeMap::new(),
            description: BTreeMap::new(),
            space: space.to_owned(),
            project: space.to_owned(),
            audience: Audience::Public,
            allowed_projects: Vec::new(),
            representations: vec![Representation::NgsiLd, Representation::Mcp],
            rate_limit: None,
            file_limits: None,
            hidden_attributes: Default::default(),
            projection: None,
            base_path: format!("/cs/{space}"),
            models: Vec::new(),
            view_mapping: None,
            policies: vec![granted_to(space, role)],
        }),
        title: BTreeMap::from([("en".to_owned(), format!("The {space} space"))]),
        description: BTreeMap::from([("en".to_owned(), format!("Everything about {space}"))]),
        is_sandbox: false,
        default_locale: Some("sk".to_owned()),
    }
}

/// One space anonymous callers reach and two that only an accountant does.
fn three_spaces() -> Vec<Space> {
    vec![
        space_named(OPEN, "public"),
        space_named(BOOKS, "accountant"),
        space_named(PAYROLL, "accountant"),
    ]
}

struct Deployment {
    realm: common::Realm,
    spaces: Vec<Space>,
}

impl Deployment {
    fn new(spaces: Vec<Space>) -> Self {
        Self {
            realm: common::Realm::new(),
            spaces,
        }
    }

    fn router(&self) -> axum::Router {
        router(Arc::new(
            Gateway::new(
                Broker::new("http://127.0.0.1:1"),
                Box::new(PolicyPdp),
                "banskabystrica.sk",
            )
            .serve_spaces(self.spaces.clone())
            .authenticate(
                Arc::new(self.realm.verifier()),
                context_gateway::auth::accounts::ServiceAccounts::new(),
                Some(HOST.to_owned()),
            ),
        ))
    }

    /// A person's token: one audience, and the realm roles it carries.
    fn person(&self, audience: &str, roles: &[&str]) -> String {
        self.realm.mint(&json!({
            "iss": common::ISSUER,
            "sub": "0d1f4b3c-8f21-4f5a-9b3e-2f1d6c7a8b90",
            "aud": audience,
            "azp": "portal",
            "preferred_username": "jana.kovacova",
            "exp": common::in_seconds(300),
            "iat": common::in_seconds(-10),
            "realm_access": { "roles": roles },
        }))
    }
}

async fn catalog(app: axum::Router, request: Request<Body>) -> (StatusCode, String, String) {
    let response = app.oneshot(request).await.expect("the gateway answers");
    let status = response.status();
    let media = response
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let body = axum::body::to_bytes(response.into_body(), 512 * 1024)
        .await
        .expect("a readable body");
    (status, media, String::from_utf8_lossy(&body).into_owned())
}

fn get() -> Request<Body> {
    Request::builder()
        .uri("/cs")
        .body(Body::empty())
        .expect("a request")
}

fn get_with_token(token: &str) -> Request<Body> {
    Request::builder()
        .uri("/cs")
        .header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .expect("a request")
}

fn names(body: &str) -> Vec<String> {
    let catalog: Value = serde_json::from_str(body).expect("json-ld");
    catalog["dcat:dataset"]
        .as_array()
        .expect("datasets")
        .iter()
        .filter_map(|dataset| dataset["dct:identifier"].as_str())
        .map(str::to_owned)
        .collect()
}

/// The first case: a token is minted for one resource, so holding the accountant role lists the one
/// space the token names and no other — not the second space the same role grants, and not even the
/// space anonymous callers read, because a token that does not verify against a space is a space
/// this caller cannot open (SP-11, ADR-N-019). It errs closed: the public space comes back the
/// moment the same caller asks for it without a token.
#[tokio::test]
async fn a_token_bound_to_one_space_lists_that_space_alone() {
    let deployment = Deployment::new(three_spaces());
    let token = deployment.person(BOOKS, &["accountant"]);

    let (status, _, body) = catalog(deployment.router(), get_with_token(&token)).await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(names(&body), vec![BOOKS.to_owned()], "{body}");
    assert!(
        !body.contains(PAYROLL),
        "the second accountant space is absent: {body}"
    );

    let (_, _, anonymous) = catalog(deployment.router(), get()).await;
    assert_eq!(names(&anonymous), vec![OPEN.to_owned()], "{anonymous}");
}

/// The edge audience is the one exception and it is the Policy's business, not the audience's: a
/// session token from the edge is accepted on every space, and what it lists is what its roles are
/// granted (PF-46).
#[tokio::test]
async fn an_edge_session_lists_what_its_roles_grant_and_no_more() {
    let deployment = Deployment::new(three_spaces());
    let accountant = deployment.person("context-gateway", &["accountant"]);
    let nobody = deployment.person("context-gateway", &["citizen"]);

    let (_, _, all) = catalog(deployment.router(), get_with_token(&accountant)).await;
    let (_, _, none) = catalog(deployment.router(), get_with_token(&nobody)).await;

    // The catalog is in the resolver's own order, which is the names sorted.
    assert_eq!(
        names(&all),
        vec![PAYROLL.to_owned(), OPEN.to_owned(), BOOKS.to_owned()],
        "{all}"
    );
    assert_eq!(names(&none), vec![OPEN.to_owned()], "{none}");
    assert!(!none.contains(BOOKS), "{none}");
}

/// A token that does not verify is not an error here, it is the narrowing: every space refuses that
/// token, so the catalog is empty — not the anonymous catalog, and not a `401` that would say which
/// of the two the caller got wrong. It errs closed, and the same caller reaches the public space
/// again by not presenting the token at all.
#[tokio::test]
async fn a_token_that_does_not_verify_discovers_nothing() {
    let deployment = Deployment::new(three_spaces());
    let other_realm = common::Realm::new();
    let expired = deployment.realm.mint(&json!({
        "iss": common::ISSUER,
        "sub": "0d1f4b3c-8f21-4f5a-9b3e-2f1d6c7a8b90",
        "aud": BOOKS,
        "azp": "portal",
        "preferred_username": "jana.kovacova",
        "exp": common::in_seconds(-300),
        "iat": common::in_seconds(-600),
        "realm_access": { "roles": ["accountant"] },
    }));
    let foreign = other_realm.mint(&json!({
        "iss": common::ISSUER,
        "sub": "0d1f4b3c-8f21-4f5a-9b3e-2f1d6c7a8b90",
        "aud": BOOKS,
        "azp": "portal",
        "preferred_username": "jana.kovacova",
        "exp": common::in_seconds(300),
        "iat": common::in_seconds(-10),
        "realm_access": { "roles": ["accountant"] },
    }));
    let unknown_kid = deployment.realm.mint_with_kid(
        "borrowed-key",
        &json!({
            "iss": common::ISSUER,
            "sub": "0d1f4b3c-8f21-4f5a-9b3e-2f1d6c7a8b90",
            "aud": BOOKS,
            "azp": "portal",
            "preferred_username": "jana.kovacova",
            "exp": common::in_seconds(300),
            "iat": common::in_seconds(-10),
            "realm_access": { "roles": ["accountant"] },
        }),
    );

    let (anonymous_status, _, anonymous) = catalog(deployment.router(), get()).await;
    assert_eq!(anonymous_status, StatusCode::OK);
    assert_eq!(names(&anonymous), vec![OPEN.to_owned()], "{anonymous}");

    for (what, token) in [
        ("expired", expired),
        ("another realm", foreign),
        ("an unpublished key", unknown_kid),
        ("not a token at all", "not-a-token".to_owned()),
        ("two dots", "..".to_owned()),
    ] {
        let (status, _, body) = catalog(deployment.router(), get_with_token(&token)).await;
        assert_eq!(status, StatusCode::OK, "{what}: {body}");
        assert!(names(&body).is_empty(), "{what}: {body}");
        assert_ne!(
            body, anonymous,
            "{what}: and it is not the anonymous answer"
        );
        for secret in [BOOKS, PAYROLL, "accountant"] {
            assert!(
                !body.contains(secret),
                "{what}: {secret} came out in {body}"
            );
        }
    }
}

/// A space the caller may not discover leaves no trace: not its name, not its title, not its
/// description, and no hint that something was left out.
#[tokio::test]
async fn a_space_left_out_leaves_no_trace_in_the_document() {
    let deployment = Deployment::new(three_spaces());

    let (_, _, body) = catalog(deployment.router(), get()).await;

    for secret in [
        BOOKS,
        PAYROLL,
        "The uctovnictvo space",
        "Everything about mzdy",
        "accountant",
        "denied",
        "restricted",
    ] {
        assert!(!body.contains(secret), "{secret} came out in {body}");
    }
}

/// A deployment with no space the caller may discover is an empty catalog, not a `404` and not a
/// refusal: the surface exists, and it is empty.
#[tokio::test]
async fn nothing_to_discover_is_an_empty_catalog() {
    for spaces in [Vec::new(), vec![space_named(BOOKS, "accountant")]] {
        let deployment = Deployment::new(spaces);
        let (status, media, body) = catalog(deployment.router(), get()).await;

        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(media, "application/ld+json");
        let document: Value = serde_json::from_str(&body).expect("json-ld");
        assert_eq!(document["@type"], json!("dcat:Catalog"));
        assert_eq!(document["dcat:dataset"], json!([]), "{body}");
    }
}

/// The catalog is JSON-LD for everybody: it is one document for a program to read, and an `Accept`
/// naming something else does not make it a page or a `406`.
#[tokio::test]
async fn the_catalog_is_json_ld_whatever_the_caller_accepts() {
    let deployment = Deployment::new(three_spaces());

    for accept in [
        "text/html",
        "text/turtle",
        "application/pdf",
        "*/*",
        "",
        "application/ld+json",
    ] {
        let request = Request::builder()
            .uri("/cs")
            .header(axum::http::header::ACCEPT, accept)
            .body(Body::empty())
            .expect("a request");
        let (status, media, body) = catalog(deployment.router(), request).await;

        assert_eq!(status, StatusCode::OK, "{accept:?}: {body}");
        assert_eq!(media, "application/ld+json", "{accept:?}");
    }
}

/// Two `Authorization` headers are one caller's first token and never the union of both: a second
/// header cannot add a space the first does not reach.
#[tokio::test]
async fn a_second_authorization_header_adds_nothing() {
    let deployment = Deployment::new(three_spaces());
    let books = deployment.person(BOOKS, &["accountant"]);
    let payroll = deployment.person(PAYROLL, &["accountant"]);

    let request = Request::builder()
        .uri("/cs")
        .header(axum::http::header::AUTHORIZATION, format!("Bearer {books}"))
        .header(
            axum::http::header::AUTHORIZATION,
            format!("Bearer {payroll}"),
        )
        .body(Body::empty())
        .expect("a request");
    let (status, _, body) = catalog(deployment.router(), request).await;

    assert_eq!(status, StatusCode::OK, "{body}");
    let listed = names(&body);
    assert!(
        !(listed.contains(&BOOKS.to_owned()) && listed.contains(&PAYROLL.to_owned())),
        "two tokens are not both: {listed:?}"
    );
}

/// What a client says on the way in about the tenant or about narrowing changes nothing here: the
/// catalog is decided by the token and the policies, and by nothing a header claims.
#[tokio::test]
async fn no_header_a_client_sends_changes_the_catalog() {
    let deployment = Deployment::new(three_spaces());
    let (_, _, plain) = catalog(deployment.router(), get()).await;

    for (name, value) in [
        ("NGSILD-Tenant", BOOKS),
        ("NGSILD-Results-Restricted", "true"),
        ("X-Forwarded-User", "accountant"),
        ("NGSILD-Warning", "none"),
    ] {
        let request = Request::builder()
            .uri("/cs")
            .header(name, value)
            .body(Body::empty())
            .expect("a request");
        let (status, _, body) = catalog(deployment.router(), request).await;

        assert_eq!(status, StatusCode::OK, "{name}: {body}");
        assert_eq!(body, plain, "{name} changed the catalog");
    }
}

/// Only a read reaches the catalog: a write method is not an answer here.
#[tokio::test]
async fn a_write_method_on_the_catalog_is_refused() {
    let deployment = Deployment::new(three_spaces());

    for method in ["POST", "PUT", "PATCH", "DELETE"] {
        let request = Request::builder()
            .method(method)
            .uri("/cs")
            .body(Body::empty())
            .expect("a request");
        let (status, _, body) = catalog(deployment.router(), request).await;

        assert!(
            status == StatusCode::METHOD_NOT_ALLOWED || status == StatusCode::NOT_FOUND,
            "{method}: {status} {body}"
        );
    }
}

/// Two callers share this URL and are answered differently, so no shared cache may replay one
/// caller's catalog to another, and the internal tenant never comes out (R9, SP-05, T-2261).
#[tokio::test]
async fn the_catalog_is_never_stored_in_a_shared_cache() {
    let deployment = Deployment::new(three_spaces());
    let response = deployment
        .router()
        .oneshot(get())
        .await
        .expect("the gateway answers");

    let headers = response.headers();
    let cache = headers
        .get(axum::http::header::CACHE_CONTROL)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    assert!(cache.contains("private"), "{cache}");
    assert!(
        cache.contains("no-store") || cache.contains("no-cache"),
        "{cache}"
    );
    assert!(
        headers
            .get(axum::http::header::VARY)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|vary| vary.contains("Authorization")),
        "{headers:?}"
    );
    assert!(headers.get("ngsild-tenant").is_none(), "{headers:?}");
    assert!(
        headers.get("ngsild-results-restricted").is_none(),
        "nobody asked to be told about narrowing: {headers:?}"
    );
}

/// Every entry carries the same fields, the IRIs are the deployment's own, and a space's title is a
/// language map rather than one of its locales picked for the reader.
#[tokio::test]
async fn every_entry_is_a_dataset_with_a_resolvable_iri() {
    let deployment = Deployment::new(three_spaces());
    let (status, _, body) = catalog(deployment.router(), get()).await;
    assert_eq!(status, StatusCode::OK);

    let document: Value = serde_json::from_str(&body).expect("json-ld");
    assert_eq!(document["@id"], json!(format!("{HOST}/cs")));
    for dataset in document["dcat:dataset"].as_array().expect("datasets") {
        assert_eq!(dataset["@type"], json!("dcat:Dataset"), "{dataset}");
        let name = dataset["dct:identifier"].as_str().expect("an identifier");
        assert_eq!(
            dataset["@id"],
            json!(format!("{HOST}/cs/{name}")),
            "{dataset}"
        );
        assert!(
            dataset["dct:title"].is_array() || dataset["dct:title"].is_string(),
            "{dataset}"
        );
    }
}

/// A catalog of many spaces is the whole list, in one stable order, and the same bytes twice: a
/// client that pages or diffs it reads the same document.
#[tokio::test]
async fn many_spaces_are_all_listed_in_a_stable_order() {
    let spaces: Vec<Space> = (0..50)
        .map(|index| space_named(&format!("space-{index:02}"), "public"))
        .collect();
    let deployment = Deployment::new(spaces);

    let (status, _, first) = catalog(deployment.router(), get()).await;
    let (_, _, again) = catalog(deployment.router(), get()).await;

    assert_eq!(status, StatusCode::OK);
    let listed = names(&first);
    assert_eq!(listed.len(), 50, "nothing is truncated: {listed:?}");
    assert_eq!(first, again, "the same bytes twice");
    let mut sorted = listed.clone();
    sorted.sort();
    assert_eq!(listed, sorted, "and one order: {listed:?}");
}

/// A sandbox is discoverable like any other space; what it is, is the record's business (PF-19).
#[tokio::test]
async fn a_sandbox_space_is_listed_like_any_other() {
    let mut sandbox = space_named("sandbox-1", "public");
    sandbox.is_sandbox = true;
    let deployment = Deployment::new(vec![space_named(OPEN, "public"), sandbox]);

    let (status, _, body) = catalog(deployment.router(), get()).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        names(&body),
        vec![OPEN.to_owned(), "sandbox-1".to_owned()],
        "{body}"
    );
}
