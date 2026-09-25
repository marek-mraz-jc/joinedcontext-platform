//! T-0178: the transfer token a data space agreement issues (DS-01, DS-02, DS-11, DS-12).
//!
//! The connector is an addon with no credential of its own, so everything a data space
//! consumer may do has to come from the agreement and from nothing else. Three properties
//! carry that, and they are what is asserted here: the token is bound to one agreement and
//! one participant, it is short-lived by construction rather than by luck, and the moment
//! the agreement stops being served every token under it stops working.

mod common;

use axum::body::Body;
use axum::extract::Request;
use axum::http::{Method, StatusCode};
use axum::routing::any;
use axum::Router;
use context_gateway::app::{router, Gateway};
use context_gateway::auth::accounts::ServiceAccounts;
use context_gateway::auth::dataspace_token::{agreements_of, subject, Agreements, Refused};
use context_gateway::auth::token::Claims;
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::Endpoint;
use jc_core::kinds::{Audience, PolicySpec, Representation};
use jcctl::loader::Repository;
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

const SLUG: &str = "d7m2xq9vkt4zc6wrb8shj5nfp3";
const OTHER_SLUG: &str = "z3f8kq5vmt7xc2wrb9shd4njp6";
const PROJECT: &str = "banskabystrica";
const SPACE: &str = "ovzdusie";
const DOMAIN: &str = "banskabystrica.sk";
/// The consumer, as DS-04 names one: the `did:web` of its verified organization domain.
const CONSUMER: &str = "did:web:helsinki.fi";
const AGREEMENT: &str = "urn:uuid:9a1f-air-quality";
/// The Keycloak client the connector obtains transfer tokens with (DS-01).
const CONNECTOR: &str = "banskabystrica-dataspace-connector";

fn station() -> Value {
    json!({
        "id": "urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:ovzdusie:senzor-01",
        "type": "AirQualityObserved",
        "temperature": { "type": "Property", "value": 19.5 }
    })
}

async fn broker() -> String {
    let app = Router::new().fallback(any(
        |_: Request| async move { axum::Json(json!([station()])) },
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a free port");
    let address = listener.local_addr().expect("the bound address");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{address}")
}

/// The grant the ODRL mapper compiles from an agreement: assigned to the consumer's DID.
fn policy(assignee: &str) -> PolicySpec {
    serde_norway::from_str(&format!(
        "contextSpaceRef: {SPACE}\n\
         assigner: did:web:{DOMAIN}\n\
         assignee: {assignee}\n\
         operations: [queryEntity, retrieveEntity]\n\
         information:\n\
         \x20 - entities:\n\
         \x20     - type: AirQualityObserved\n\
         \x20   propertyNames: [temperature]\n"
    ))
    .expect("the policy spec parses")
}

fn endpoint(slug: &str, assignee: &str) -> Endpoint {
    Endpoint {
        roles: Default::default(),
        slug: slug.to_owned(),
        title: std::collections::BTreeMap::new(),
        description: std::collections::BTreeMap::new(),
        space: SPACE.to_owned(),
        project: PROJECT.to_owned(),
        audience: Audience::Organization,
        allowed_projects: Vec::new(),
        representations: vec![Representation::NgsiLd],
        rate_limit: None,
        file_limits: None,
        hidden_attributes: Default::default(),
        projection: None,
        base_path: format!("/api/endpoint/{slug}"),
        models: Vec::new(),
        view_mapping: None,
        catalog: None,
        policies: vec![policy(assignee)],
    }
}

/// A directory of this test's own, removed by the operating system rather than by a `Drop`
/// nobody would see fail.
fn tempdir() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "jc-ds-{}-{:?}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("a clock after 1970")
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("a temporary directory");
    dir
}

/// The agreement as a manifest on disk, so the table under test is the one the gateway
/// really builds from a repository and not one the test invented.
fn written(role: &str, state: &str, participant: &str, validity: &str) -> Agreements {
    let dir = tempdir();
    let agreements = dir
        .join("projects")
        .join(PROJECT)
        .join("dataspace")
        .join("agreements");
    std::fs::create_dir_all(&agreements).expect("the repository directory");
    std::fs::write(
        agreements.join("air-quality.yaml"),
        format!(
            "apiVersion: joinedcontext.com/v1alpha1\n\
             kind: DataAgreement\n\
             metadata:\n  name: air-quality\n  namespace: {PROJECT}\n\
             spec:\n  role: {role}\n  offerRef: {{ kind: DataOffer, name: air-quality }}\n\
             \x20 remoteParticipant: {participant}\n\
             \x20 agreementId: {AGREEMENT}\n\
             \x20 state: {state}\n\
             \x20 validity: {validity}\n"
        ),
    )
    .expect("the agreement is written");
    agreements_of(&Repository::load(&dir).expect("the repository loads"))
}

/// An agreement that is being served right now.
fn serving_now() -> Agreements {
    written(
        "provider",
        "finalized",
        CONSUMER,
        "{ from: 2026-01-01T00:00:00Z, to: 2027-01-01T00:00:00Z }",
    )
}

/// The claims the connector puts in a transfer token.
fn transfer(audience: &str, lifetime: i64, participant: &str, agreement: &str) -> Value {
    json!({
        "iss": common::ISSUER,
        "sub": "service-account-dataspace-connector",
        "aud": audience,
        "azp": CONNECTOR,
        "iat": common::in_seconds(-10),
        "exp": common::in_seconds(lifetime),
        "agreementId": agreement,
        "participant": participant,
    })
}

/// One read through the endpoint with the given token.
async fn read(agreements: Agreements, assignee: &str, claims: &Value) -> StatusCode {
    let realm = common::Realm::new();
    let token = realm.mint(claims);
    let gateway = Arc::new(
        Gateway::new(Broker::new(broker().await), Box::new(PolicyPdp), DOMAIN)
            .authenticate(Arc::new(realm.verifier()), ServiceAccounts::new(), None)
            .serve([endpoint(SLUG, assignee), endpoint(OTHER_SLUG, assignee)]),
    );
    gateway.replace_agreements(agreements);

    router(gateway)
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(format!(
                    "/api/endpoint/{SLUG}/ngsi-ld/v1/entities?type=AirQualityObserved"
                ))
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .expect("a request"),
        )
        .await
        .expect("the gateway answers")
        .status()
}

const DID_ASSIGNEE: &str = "{ kind: did, id: did:web:helsinki.fi }";

#[tokio::test]
async fn a_transfer_token_reads_as_the_consumer_did_and_nothing_else() {
    let status = read(
        serving_now(),
        DID_ASSIGNEE,
        &transfer(SLUG, 300, CONSUMER, AGREEMENT),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

/// DS-01, DS-02: the connector obtains the token, so its `azp` names an account with roles
/// of its own. Resolving it would hand a data space consumer the connector's grants.
#[tokio::test]
async fn the_connectors_own_account_grants_nothing_under_a_transfer_token() {
    let status = read(
        serving_now(),
        "{ kind: serviceAccount, id: dataspace-connector }",
        &transfer(SLUG, 300, CONSUMER, AGREEMENT),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "a grant to the connector is not a grant to the participant it fetched a token for"
    );
}

/// DS-02: the audience is one Endpoint (RFC 8707), which the ordinary verifier enforces —
/// asserted here because it is the binding the whole transfer rests on.
#[tokio::test]
async fn a_transfer_token_minted_for_another_endpoint_is_refused() {
    let status = read(
        serving_now(),
        DID_ASSIGNEE,
        &transfer(OTHER_SLUG, 300, CONSUMER, AGREEMENT),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_token_minted_to_live_longer_than_fifteen_minutes_is_refused_while_still_valid() {
    let status = read(
        serving_now(),
        DID_ASSIGNEE,
        &transfer(SLUG, 60 * 60, CONSUMER, AGREEMENT),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "DS-11 is a property of the token, not of the moment it is presented"
    );
}

/// DS-12: terminating the agreement revokes the tokens under it, without anything having to
/// find those tokens — they name an agreement the gateway no longer serves.
#[tokio::test]
async fn a_terminated_agreement_refuses_its_outstanding_tokens() {
    let claims = transfer(SLUG, 300, CONSUMER, AGREEMENT);
    assert_eq!(
        read(serving_now(), DID_ASSIGNEE, &claims).await,
        StatusCode::OK
    );

    let terminated = written(
        "provider",
        "terminated",
        CONSUMER,
        "{ from: 2026-01-01T00:00:00Z, to: 2027-01-01T00:00:00Z }",
    );
    assert_eq!(
        read(terminated, DID_ASSIGNEE, &claims).await,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn an_agreement_past_its_validity_window_refuses() {
    let expired = written(
        "provider",
        "finalized",
        CONSUMER,
        "{ from: 2024-01-01T00:00:00Z, to: 2024-06-01T00:00:00Z }",
    );
    let status = read(
        expired,
        DID_ASSIGNEE,
        &transfer(SLUG, 300, CONSUMER, AGREEMENT),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_token_naming_a_participant_the_agreement_does_not_is_refused() {
    let status = read(
        serving_now(),
        DID_ASSIGNEE,
        &transfer(SLUG, 300, "did:web:someone.else.fi", AGREEMENT),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_token_naming_an_agreement_nobody_declared_is_refused() {
    let status = read(
        serving_now(),
        DID_ASSIGNEE,
        &transfer(SLUG, 300, CONSUMER, "urn:uuid:invented"),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

/// A consumer-role agreement is a token *this* platform holds to read somebody else's
/// endpoint. It never authorises a read here, whatever its state.
#[tokio::test]
async fn a_consumer_role_agreement_authorises_nothing_here() {
    let consuming = written(
        "consumer",
        "finalized",
        CONSUMER,
        "{ from: 2026-01-01T00:00:00Z, to: 2027-01-01T00:00:00Z }",
    );
    let status = read(
        consuming,
        DID_ASSIGNEE,
        &transfer(SLUG, 300, CONSUMER, AGREEMENT),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

fn claims_of(value: &Value) -> Claims {
    serde_json::from_value(value.clone()).expect("the claims parse")
}

/// DS-13: the audit line reads `subject.agreement`, so the binding has to survive as far as
/// the subject and not stop at the check.
#[test]
fn the_agreement_travels_with_the_subject_the_audit_line_reads() {
    let established = subject(
        &claims_of(&transfer(SLUG, 300, CONSUMER, AGREEMENT)),
        &serving_now(),
        &endpoint(SLUG, DID_ASSIGNEE),
        context_gateway::pdp::now(),
    )
    .expect("the token establishes the consumer");

    assert_eq!(established.agreement.as_deref(), Some(AGREEMENT));
    assert_eq!(established.did.as_deref(), Some(CONSUMER));
    assert_eq!(established.service_account, None, "never the connector");
    assert!(
        established.roles.is_empty() && established.groups.is_empty(),
        "an agreement grants through Policy entities, not through roles"
    );
}

#[test]
fn a_token_without_an_issue_time_has_no_measurable_lifetime() {
    let mut claims = transfer(SLUG, 300, CONSUMER, AGREEMENT);
    claims.as_object_mut().expect("an object").remove("iat");

    assert_eq!(
        subject(
            &claims_of(&claims),
            &serving_now(),
            &endpoint(SLUG, DID_ASSIGNEE),
            context_gateway::pdp::now(),
        ),
        Err(Refused::NoLifetime)
    );
}

#[test]
fn a_token_naming_an_agreement_but_no_participant_is_incomplete() {
    let mut claims = transfer(SLUG, 300, CONSUMER, AGREEMENT);
    claims
        .as_object_mut()
        .expect("an object")
        .remove("participant");

    assert_eq!(
        subject(
            &claims_of(&claims),
            &serving_now(),
            &endpoint(SLUG, DID_ASSIGNEE),
            context_gateway::pdp::now(),
        ),
        Err(Refused::Incomplete)
    );
}

/// An endpoint in a project the agreement does not reach: the audience binds the token to
/// one Endpoint, this binds it to the project the agreement was negotiated in.
#[test]
fn an_endpoint_outside_the_agreements_project_is_forbidden() {
    let mut elsewhere = endpoint(SLUG, DID_ASSIGNEE);
    elsewhere.project = "helsinki".to_owned();
    elsewhere.audience = Audience::ProjectList;

    assert_eq!(
        subject(
            &claims_of(&transfer(SLUG, 300, CONSUMER, AGREEMENT)),
            &serving_now(),
            &elsewhere,
            context_gateway::pdp::now(),
        ),
        Err(Refused::OutOfProject(AGREEMENT.to_owned()))
    );
}
