//! AG-52, EP-26 — whose fault the refusal is.
//!
//! `auth::authenticate` answers five failures. Three are the caller's: a run id that is not shaped
//! like one, an id no active run holds, a ticket that does not verify. Two are the platform's: the
//! Portal could not be read, and the stored ticket hash is not a hash. All five used to answer
//! `401 invalid run credentials`, so a person asking the assistant a question was told their
//! credentials were wrong for a fault they had no part in and could not act on, and the operator
//! learned the difference only from a log nobody was told to read (T-2418, filed from the demo
//! blocker T-2420).
//!
//! The two properties these cases hold together:
//!
//! * a failure of ours is a `503` that says so, with the cause in the log and not in the body;
//! * the three credential refusals stay byte-identical, because a pair of answers that differ is
//!   an oracle for which run ids are live (T-2285) — a run id is a workspace's branch name and its
//!   mesh identity.

use agent_proxy::auth::authenticate;
use agent_proxy::config::Config;
use agent_proxy::inject::CredentialManager;
use agent_proxy::runs::{RunContext, RunResolver};
use argon2::{password_hash::SaltString, Argon2, PasswordHasher};
use axum::http::HeaderMap;
use std::sync::Arc;
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, ResponseTemplate};

const RUN: &str = "e3b0c442-98fc-1c14-9afb-4c7b2756a120";
const TICKET: &str = "secret-ticket-123";
/// A run id of the same shape that no run holds.
const OTHER_RUN: &str = "aaaaaaaa-98fc-1c14-9afb-4c7b2756a120";

fn hash_of(ticket: &str) -> String {
    let salt = SaltString::generate(&mut argon2::password_hash::rand_core::OsRng);
    Argon2::default()
        .hash_password(ticket.as_bytes(), &salt)
        .expect("the ticket hashes")
        .to_string()
}

fn run_with(ticket_hash: String) -> RunContext {
    RunContext {
        id: RUN.to_owned(),
        project: "helsinki".to_owned(),
        app_name: "assistant".to_owned(),
        endpoint_slug: "scsd2eehkx42n53z2zyd6vshfh7s7irf".to_owned(),
        endpoint_slugs: vec![],
        allows_write: false,
        branch: format!("agent/app-assistant/{RUN}"),
        path_prefix: "projects/helsinki/apps/assistant/".to_owned(),
        repository: None,
        status: "building".to_owned(),
        ticket_hash,
        max_tokens: 1_000,
        allowed_hosts: vec![],
        requests_per_minute: 100,
        steps_per_run: 0,
        max_response_bytes: 1_048_576,
        max_egress_bytes_per_run: 0,
        created_by: "demo.steward@hel.fi".to_owned(),
        model_name: "claude-3-7".to_owned(),
        reasoning_effort: None,
    }
}

fn config_with_portal(portal: &str) -> Config {
    let portal = portal.to_owned();
    Config::from_lookup(|key| match key {
        "JC_PROXY_BIND" => Some("127.0.0.1:0".to_owned()),
        "JC_GATEWAY_BASE" => Some("http://context-gateway.invalid:8080".to_owned()),
        "JC_MODEL_BASE" => Some("http://model-provider.invalid".to_owned()),
        "JC_FORGE_BASE" => Some("http://gitea-http.invalid:3000".to_owned()),
        "JC_PORTAL_BASE" => Some(portal.clone()),
        "JC_MODEL_KEY" => Some("mock-model-key".to_owned()),
        "JC_FORGE_TOKEN" => Some("mock-forge-token".to_owned()),
        _ => None,
    })
    .expect("the test configuration parses")
}

/// A resolver that holds this one run and asks `portal` about anything else.
fn resolver_holding(run: RunContext, portal: &str) -> RunResolver {
    RunResolver::with_cached_at(portal.parse().expect("the portal base is a URL"), run)
}

/// A resolver that holds nothing, so every id is looked up on `portal`.
fn resolver_asking(portal: &str, config: &Config) -> RunResolver {
    RunResolver::new(
        portal.parse().expect("the portal base is a URL"),
        CredentialManager::new(Arc::new(config.clone())),
    )
}

fn credentials(run: &str, ticket: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert("x-jc-run", run.parse().expect("a header value"));
    headers.insert("x-jc-ticket", ticket.parse().expect("a header value"));
    headers
}

/// The answer as `(status, detail)`, which is everything a caller reads.
async fn refusal(headers: HeaderMap, resolver: &RunResolver, config: &Config) -> (u16, String) {
    let problem = authenticate(&headers, resolver, config)
        .await
        .expect_err("these cases are all refusals");
    (problem.status, problem.detail.clone().unwrap_or_default())
}

/// A Portal answering `status` to the run lookup, and the proxy that asks it.
async fn portal_answering(status: u16) -> (MockServer, Config) {
    let portal = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(status))
        .mount(&portal)
        .await;
    let config = config_with_portal(&portal.uri());
    (portal, config)
}

// -------------------------------------------------------------------------------------------
// The platform's own failures: a 503 that says whose fault it is.
// -------------------------------------------------------------------------------------------

/// AG-52: the Portal is not there. Nothing about the presented credential was judged, so the
/// caller is not told their credential is wrong.
#[tokio::test]
async fn a_portal_that_cannot_be_reached_is_not_the_callers_fault() {
    // A name that resolves nowhere: the lookup fails in transport, before any status exists.
    let config = config_with_portal("http://portal.invalid:8080");
    let resolver = resolver_asking("http://portal.invalid:8080", &config);

    let (status, detail) = refusal(credentials(RUN, TICKET), &resolver, &config).await;
    assert_eq!(status, 503, "a failure of ours is not a 401");
    assert!(
        !detail.contains("invalid run credentials"),
        "the caller is not blamed for it: {detail}"
    );
    assert!(
        detail.contains("not of your credentials"),
        "and is told so in a sentence they can act on: {detail}"
    );
}

/// The Portal is reachable and refuses this proxy's own workload token, or fails inside: both are
/// non-2xx answers the proxy cannot read a run out of, and neither says anything about the
/// caller's credential.
#[tokio::test]
async fn a_portal_that_refuses_this_proxy_is_not_the_callers_fault() {
    for status in [401, 403, 500, 502] {
        let (_portal, config) = portal_answering(status).await;
        let resolver = resolver_asking(config.portal_base.as_ref(), &config);

        let (answered, detail) = refusal(credentials(RUN, TICKET), &resolver, &config).await;
        assert_eq!(
            answered, 503,
            "a Portal answering {status} is a platform failure, not a bad credential"
        );
        assert!(!detail.contains("invalid run credentials"), "{detail}");
    }
}

/// A Portal that answers a run this proxy cannot parse is the same failure: the run's context
/// could not be read.
#[tokio::test]
async fn a_run_record_that_cannot_be_read_is_not_the_callers_fault() {
    let portal = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200).set_body_string("{\"id\": \"only-an-id\"}"))
        .mount(&portal)
        .await;
    let config = config_with_portal(&portal.uri());
    let resolver = resolver_asking(&portal.uri(), &config);

    let (status, _) = refusal(credentials(RUN, TICKET), &resolver, &config).await;
    assert_eq!(status, 503);
}

/// The run is there and its stored hash is not a hash, so no ticket can be verified against it.
/// The caller presented a perfectly good ticket and gets a `503`, not a `401`.
#[tokio::test]
async fn a_stored_hash_that_is_not_a_hash_is_not_the_callers_fault() {
    let config = config_with_portal("http://portal.invalid:8080");
    let resolver = resolver_holding(
        run_with("this is not a PHC string".to_owned()),
        "http://portal.invalid:8080",
    );

    let (status, detail) = refusal(credentials(RUN, TICKET), &resolver, &config).await;
    assert_eq!(status, 503, "an unusable stored hash is ours to fix");
    assert!(!detail.contains("invalid run credentials"), "{detail}");
}

// -------------------------------------------------------------------------------------------
// The caller's own failures: one answer, unchanged.
// -------------------------------------------------------------------------------------------

/// A ticket that does not verify is still `401 invalid run credentials`.
#[tokio::test]
async fn a_ticket_that_does_not_verify_is_still_the_callers_fault() {
    let config = config_with_portal("http://portal.invalid:8080");
    let resolver = resolver_holding(run_with(hash_of(TICKET)), "http://portal.invalid:8080");

    let (status, detail) = refusal(credentials(RUN, "not-the-ticket"), &resolver, &config).await;
    assert_eq!(status, 401);
    assert_eq!(detail, "invalid run credentials");
}

/// A run id that is not shaped like one never reaches the Portal, and answers the same sentence.
#[tokio::test]
async fn a_run_id_that_is_not_a_name_is_still_the_callers_fault() {
    let config = config_with_portal("http://portal.invalid:8080");
    let resolver = resolver_holding(run_with(hash_of(TICKET)), "http://portal.invalid:8080");

    for id in ["../../admin", "a b", &"a".repeat(65)] {
        let (status, detail) = refusal(credentials(id, TICKET), &resolver, &config).await;
        assert_eq!(status, 401, "{id:?}");
        assert_eq!(detail, "invalid run credentials", "{id:?}");
    }
}

/// EP-26, R20, T-2285: the two answers a prober could compare are byte-identical, so no pair of
/// them says which run ids are live. The 404 comes from the Portal, which is where "no such run"
/// is decided; the wrong ticket is decided here.
#[tokio::test]
async fn an_unknown_run_and_a_wrong_ticket_answer_identically() {
    let (portal, config) = portal_answering(404).await;
    let unknown = resolver_asking(&portal.uri(), &config);
    let known = resolver_holding(run_with(hash_of(TICKET)), &portal.uri());

    let no_such_run = refusal(credentials(OTHER_RUN, TICKET), &unknown, &config).await;
    let wrong_ticket = refusal(credentials(RUN, "not-the-ticket"), &known, &config).await;

    assert_eq!(
        no_such_run, wrong_ticket,
        "an unknown run and a wrong ticket must not be told apart"
    );
    assert_eq!(no_such_run.0, 401);
}

/// A run that has finished is a fact about the caller's own run, and answers like an unknown one
/// for the same reason: telling them apart is the oracle.
#[tokio::test]
async fn a_finished_run_answers_like_an_unknown_one() {
    let (portal, config) = portal_answering(404).await;
    let mut finished = run_with(hash_of(TICKET));
    // The four terminal states `RunResolver::check_status` names; `completed` is not one of them.
    finished.status = "expired".to_owned();
    let held = resolver_holding(finished, &portal.uri());
    let unknown = resolver_asking(&portal.uri(), &config);

    let terminal = refusal(credentials(RUN, TICKET), &held, &config).await;
    let no_such_run = refusal(credentials(OTHER_RUN, TICKET), &unknown, &config).await;

    assert_eq!(terminal, no_such_run);
    assert_eq!(terminal.0, 401);
}

/// And the two kinds never collide: what the platform answers for its own failure is not what it
/// answers for a bad credential, in status or in words.
#[tokio::test]
async fn a_platform_failure_and_a_bad_credential_never_read_alike() {
    let config = config_with_portal("http://portal.invalid:8080");
    let ours = resolver_asking("http://portal.invalid:8080", &config);
    let theirs = resolver_holding(run_with(hash_of(TICKET)), "http://portal.invalid:8080");

    let platform = refusal(credentials(RUN, TICKET), &ours, &config).await;
    let credential = refusal(credentials(RUN, "not-the-ticket"), &theirs, &config).await;

    assert_ne!(platform.0, credential.0);
    assert_ne!(platform.1, credential.1);
}
