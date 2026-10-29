//! A person's script token from `jcctl token --resource` (T-5614, PF-45, PF-46).
//!
//! The `jcctl` client's tokens name the Portal (`portal-api`) and the gateway's command-line
//! audience `jcctl-endpoint`. The second one reaches an Endpoint only together with the
//! `endpoint:{slug}` scope naming that Endpoint, which `jcctl token --resource` asks for, and the
//! Endpoint's Policy then decides by the person. A token of the Portal alone is no token here.

use crate::common;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use context_gateway::app::{router, Gateway};
use context_gateway::auth::accounts::ServiceAccounts;
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::Endpoint;
use context_gateway::store::Provenance;
use jc_core::kinds::{Audience, PolicySpec, Representation};
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

/// The Endpoint the analyst's Policy grants.
const GRANTED: &str = "a7nalystq2mzt6vhx3nbwrs5cj";
/// An Endpoint of a project of another Organization, which the analyst is no member of.
const OTHER: &str = "o7therssq2mzt6vhx3nbwrs5cj";
const PROJECT: &str = "helsinki-mobility";
const OUTSIDE: &str = "zilina";
const ANALYST: &str = "demo.analyst";

fn endpoint(slug: &str, project: &str) -> Endpoint {
    let policy: PolicySpec = serde_norway::from_str(&format!(
        "contextSpaceRef: bikes\nassigner: did:web:hel.fi\n\
         assignee: {{ kind: user, id: \"{ANALYST}\" }}\noperations: [queryEntity]\n\
         information:\n  - entities:\n      - type: BikeHireDockingStation\n"
    ))
    .expect("the policy spec parses");
    Endpoint {
        declared_types: None,
        roles: Default::default(),
        slug: slug.to_owned(),
        title: Default::default(),
        description: Default::default(),
        space: "bikes".to_owned(),
        project: project.to_owned(),
        audience: Audience::Organization,
        allowed_projects: Vec::new(),
        representations: vec![Representation::NgsiLd],
        rate_limit: None,
        creates: None,
        file_limits: None,
        hidden_attributes: Default::default(),
        projection: None,
        base_path: format!("/api/endpoint/{slug}"),
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

struct Fixture {
    realm: common::Realm,
    app: axum::Router,
    broker: common::BrokerStub,
}

async fn fixture() -> Fixture {
    let realm = common::Realm::new();
    let broker = common::BrokerStub::start(vec![json!([])]).await;
    let gateway = Gateway::new(
        Broker::new(broker.url.clone()),
        Box::new(PolicyPdp),
        "hel.fi",
    )
    .serve([endpoint(GRANTED, PROJECT), endpoint(OTHER, OUTSIDE)])
    .authenticate(
        Arc::new(realm.verifier()),
        ServiceAccounts::new(),
        Some("https://api.hel.fi".to_owned()),
    );
    gateway.replace_provenance(Provenance {
        organizations: [(PROJECT, "hel"), (OUTSIDE, "zilina")]
            .into_iter()
            .map(|(project, organization)| (project.to_owned(), organization.to_owned()))
            .collect(),
        ..Default::default()
    });
    Fixture {
        realm,
        app: router(Arc::new(gateway)),
        broker,
    }
}

/// The analyst's token as Keycloak issues it to `azp` with `aud` and `scope`.
fn person(realm: &common::Realm, azp: &str, aud: Value, scope: &str) -> String {
    realm.mint(&json!({
        "iss": common::ISSUER,
        "sub": "f:1:analyst",
        "aud": aud,
        "azp": azp,
        "scope": scope,
        "exp": common::in_seconds(300),
        "iat": common::in_seconds(-10),
        "preferred_username": ANALYST,
        "groups": [PROJECT],
    }))
}

fn cli(realm: &common::Realm, scope: &str) -> String {
    person(
        realm,
        "jcctl",
        json!(["portal-api", "jcctl-endpoint"]),
        scope,
    )
}

async fn read(fixture: &Fixture, slug: &str, token: &str) -> StatusCode {
    fixture
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
        .expect("the gateway answers")
        .status()
}

/// Done when of T-5614: the token reads the Endpoint it names and the person's Policy grants.
#[tokio::test]
async fn a_jcctl_token_for_one_endpoint_reads_it_as_the_person() {
    let fixture = fixture().await;
    let token = cli(&fixture.realm, &format!("openid endpoint:{GRANTED}"));
    assert_eq!(read(&fixture, GRANTED, &token).await, StatusCode::OK);
    assert_eq!(fixture.broker.hops().len(), 1, "the broker was asked once");
}

/// Decided by the person: on an Endpoint of a project the analyst is no member of, the token
/// that names it is refused exactly as the person's own token there, and the broker is never
/// asked. (Which entity types a granted person reads is the view's decision, T-3643.)
#[tokio::test]
async fn a_jcctl_token_for_an_endpoint_the_person_may_not_read_is_refused_as_the_person() {
    let fixture = fixture().await;
    let token = cli(&fixture.realm, &format!("openid endpoint:{OTHER}"));
    let own = person(&fixture.realm, "edge", json!(OTHER), "openid");
    let refused = read(&fixture, OTHER, &token).await;
    assert!(
        refused.is_client_error() && refused != StatusCode::UNAUTHORIZED,
        "{refused}"
    );
    assert_eq!(refused, read(&fixture, OTHER, &own).await);
    assert_eq!(fixture.broker.hops().len(), 0, "the broker was never asked");
}

/// The read goes to the broker exactly as the person's own token for that Endpoint sends it:
/// the same subject, so the same view decides (T-3643).
#[tokio::test]
async fn a_jcctl_read_is_the_persons_read() {
    let fixture = fixture().await;
    let cli_token = cli(&fixture.realm, &format!("openid endpoint:{GRANTED}"));
    let own = person(&fixture.realm, "edge", json!(GRANTED), "openid");
    assert_eq!(read(&fixture, GRANTED, &cli_token).await, StatusCode::OK);
    assert_eq!(read(&fixture, GRANTED, &own).await, StatusCode::OK);
    let hops = fixture.broker.hops();
    assert_eq!(hops.len(), 2);
    let identity = |hop: &common::Hop| {
        let mut headers: Vec<(String, String)> = hop
            .headers
            .iter()
            .filter(|(name, _)| {
                !matches!(
                    name.as_str(),
                    "traceparent"
                        | "tracestate"
                        | "authorization"
                        | "x-request-id"
                        | "jc-provenance-client"
                )
            })
            .map(|(name, value)| {
                (
                    name.to_string(),
                    value.to_str().unwrap_or_default().to_owned(),
                )
            })
            .collect();
        headers.sort();
        (
            hop.view.clone(),
            hop.query.clone(),
            hop.tenant.clone(),
            headers,
        )
    };
    assert_eq!(identity(&hops[0]), identity(&hops[1]));
}

/// One Endpoint per token: the scope of one opens no other, and neither does the hub's
/// `endpoint:*` or no endpoint scope at all.
#[tokio::test]
async fn a_jcctl_token_reaches_only_the_endpoint_its_scope_names() {
    let fixture = fixture().await;
    for scope in [
        format!("openid endpoint:{OTHER}"),
        "openid endpoint:*".to_owned(),
        "openid".to_owned(),
    ] {
        let token = cli(&fixture.realm, &scope);
        assert_eq!(
            read(&fixture, GRANTED, &token).await,
            StatusCode::UNAUTHORIZED,
            "{scope}"
        );
    }
    assert_eq!(fixture.broker.hops().len(), 0);
}

/// The Portal's audience alone, which a plain `jcctl login` token carries, is 401 on an Endpoint.
#[tokio::test]
async fn a_portal_api_only_token_is_401() {
    let fixture = fixture().await;
    let token = person(
        &fixture.realm,
        "jcctl",
        json!("portal-api"),
        &format!("openid endpoint:{GRANTED}"),
    );
    assert_eq!(
        read(&fixture, GRANTED, &token).await,
        StatusCode::UNAUTHORIZED
    );
}

/// The command-line audience holds for the `jcctl` client's tokens alone.
#[tokio::test]
async fn the_cli_audience_on_another_clients_token_is_401() {
    let fixture = fixture().await;
    let token = person(
        &fixture.realm,
        "portal-ui",
        json!(["jcctl-endpoint"]),
        &format!("openid endpoint:{GRANTED}"),
    );
    assert_eq!(
        read(&fixture, GRANTED, &token).await,
        StatusCode::UNAUTHORIZED
    );
}
