//! `POST /api/endpoint/{slug}/access/simulate` (EP-103, T-3311): the decision `access/check`
//! would give someone else, for the Portal alone.
//!
//! The one property that matters is agreement: for every subject, action and type the
//! simulator answers what the gateway answers that subject's own token, because it is the
//! same admission and the same evaluator. The rest is who may ask (the Portal's own
//! service-account token for the simulate audience, nobody else) and what the answer names
//! (the Policy that decided, which `access/check` keeps to itself on a refusal).
//!
//! Most Endpoints here have a view of another model, which no broker view serves yet, so their
//! `access/check` is the gateway's own (T-3649, `app::has_view`). Through a view the broker's
//! engine decides, and the simulator asks the engine's `access/explain` under the headers the
//! subject's own token pins (T-3881): the `view_` tests below hold that hop.
mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use context_gateway::app::{router, Gateway, PORTAL_CLIENT, SIMULATE_AUDIENCE};
use context_gateway::auth::accounts::ServiceAccounts;
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::Endpoint;
use jc_core::kinds::{Audience, PolicySpec, Representation};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

/// A public endpoint.
const OPEN: &str = "k4y7pq2mzt6vhx3nbwrs5cjd8f";
/// An endpoint for the organization's people only.
const CLOSED: &str = "p9d2wc5kzn8mth4rqvb7xj3sfy";

fn policy(yaml: &str) -> PolicySpec {
    serde_norway::from_str(yaml).expect("the policy spec parses")
}

/// Five Policies, one per way a subject is matched, each with its manifest name.
fn named_policies() -> (Vec<String>, Vec<PolicySpec>) {
    let named = [
        (
            "public-air",
            r#"contextSpaceRef: ovzdusie
assigner: did:web:banskabystrica.sk
assignee: { kind: role, id: public }
operations: [queryEntity, retrieveEntity, queryTemporal]
information:
  - entities:
      - type: AirQualityObserved
"#,
        ),
        (
            "no-history",
            r#"contextSpaceRef: ovzdusie
effect: prohibition
assigner: did:web:banskabystrica.sk
assignee: { kind: role, id: public }
operations: [queryTemporal]
"#,
        ),
        (
            "stewards",
            r#"contextSpaceRef: ovzdusie
assigner: did:web:banskabystrica.sk
assignee: { kind: role, id: data-steward }
operations: [createEntity, deleteEntity]
information:
  - entities:
      - type: InternalIncident
"#,
        ),
        (
            "ovzdusie-writes",
            r#"contextSpaceRef: ovzdusie
assigner: did:web:banskabystrica.sk
assignee: { kind: group, id: ovzdusie }
operations: [updateAttrs]
information:
  - entities:
      - type: AirQualityObserved
"#,
        ),
        (
            "ada-deletes",
            r#"contextSpaceRef: ovzdusie
assigner: did:web:banskabystrica.sk
assignee: { kind: user, id: ada }
operations: [deleteEntity]
information:
  - entities:
      - type: AirQualityObserved
"#,
        ),
    ];
    named
        .iter()
        .map(|(name, yaml)| ((*name).to_owned(), policy(yaml)))
        .unzip()
}

fn endpoint(slug: &str, audience: Audience) -> Endpoint {
    let (policy_names, policies) = named_policies();
    Endpoint {
        declared_types: None,
        roles: Default::default(),
        slug: slug.to_owned(),
        title: Default::default(),
        description: Default::default(),
        space: "ovzdusie".to_owned(),
        project: "ovzdusie".to_owned(),
        audience,
        allowed_projects: Vec::new(),
        representations: vec![Representation::NgsiLd],
        rate_limit: None,
        creates: None,
        file_limits: None,
        hidden_attributes: Default::default(),
        projection: None,
        base_path: format!("/api/endpoint/{slug}"),
        models: Vec::new(),
        view_mapping: Some(common::no_view()),
        catalog: None,
        policy_names,
        policies,
    }
}

fn app_of(realm: &common::Realm) -> axum::Router {
    router(Arc::new(
        Gateway::new(
            Broker::new("http://127.0.0.1:1"),
            Box::new(PolicyPdp),
            "banskabystrica.sk",
        )
        .serve([
            endpoint(OPEN, Audience::Public),
            endpoint(CLOSED, Audience::Organization),
        ])
        .authenticate(
            Arc::new(realm.verifier()),
            ServiceAccounts::new(),
            Some("https://bb.example.sk".to_owned()),
        ),
    ))
}

/// The Portal's own client-credentials token for the simulate audience.
fn portal_token(realm: &common::Realm) -> String {
    token_of(
        realm,
        PORTAL_CLIENT,
        &format!("service-account-{PORTAL_CLIENT}"),
    )
}

fn token_of(realm: &common::Realm, azp: &str, user: &str) -> String {
    realm.mint(&json!({
        "iss": common::ISSUER,
        "sub": user,
        "aud": SIMULATE_AUDIENCE,
        "azp": azp,
        "preferred_username": user,
        "exp": common::in_seconds(300),
        "iat": common::in_seconds(-10),
    }))
}

/// A person's own token on `slug`, as the edge would mint it.
fn person_token(realm: &common::Realm, slug: &str, who: &Value) -> String {
    realm.mint(&json!({
        "iss": common::ISSUER,
        "sub": who["user"],
        "aud": slug,
        "preferred_username": who["user"],
        "groups": who["groups"],
        "realm_access": { "roles": who["roles"] },
        "exp": common::in_seconds(300),
        "iat": common::in_seconds(-10),
    }))
}

async fn post(
    app: &axum::Router,
    path: &str,
    bearer: Option<&str>,
    body: &Value,
) -> (StatusCode, Value) {
    let mut request = Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/json");
    if let Some(bearer) = bearer {
        request = request.header("authorization", format!("Bearer {bearer}"));
    }
    let response = app
        .clone()
        .oneshot(
            request
                .body(Body::from(body.to_string()))
                .expect("a request"),
        )
        .await
        .expect("the gateway answers");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 256 * 1024)
        .await
        .expect("a readable body");
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn question(action: &str, entity_type: Option<&str>) -> Value {
    match entity_type {
        Some(entity_type) => {
            json!({ "action": { "name": action }, "resource": { "type": entity_type } })
        }
        None => json!({ "action": { "name": action } }),
    }
}

async fn simulate(
    app: &axum::Router,
    realm: &common::Realm,
    slug: &str,
    body: Value,
) -> (StatusCode, Value) {
    post(
        app,
        &format!("/api/endpoint/{slug}/access/simulate"),
        Some(&portal_token(realm)),
        &body,
    )
    .await
}

/// EP-103: for every subject, action and type, the simulator answers what the gateway answers
/// that subject's own token on `access/check`, refusals at the door included.
#[tokio::test]
async fn the_simulator_agrees_with_the_gateway_for_every_subject() {
    let realm = common::Realm::new();
    let app = app_of(&realm);
    let people = [
        json!({ "user": "vera", "groups": [], "roles": ["viewer"] }),
        json!({ "user": "stefan", "groups": ["ovzdusie"], "roles": ["data-steward"] }),
        json!({ "user": "gina", "groups": ["ovzdusie"], "roles": [] }),
        json!({ "user": "ada", "groups": ["ovzdusie"], "roles": [] }),
        json!({ "user": "otto", "groups": ["elsewhere"], "roles": [] }),
    ];
    let actions = [
        "retrieveEntity",
        "queryTemporal",
        "createEntity",
        "updateAttrs",
        "deleteEntity",
    ];
    let types = [None, Some("AirQualityObserved"), Some("InternalIncident")];
    let mut compared = 0;
    for slug in [OPEN, CLOSED] {
        for action in actions {
            for entity_type in types {
                // The public: no token at all against the subject named by nothing.
                let (status, real) = post(
                    &app,
                    &format!("/api/endpoint/{slug}/access/check"),
                    None,
                    &question(action, entity_type),
                )
                .await;
                let real_decision = status == StatusCode::OK && real["decision"] == json!(true);
                let mut asked = question(action, entity_type);
                asked["subject"] = json!({});
                let (status, simulated) = simulate(&app, &realm, slug, asked).await;
                assert_eq!(status, StatusCode::OK, "{simulated}");
                assert_eq!(
                    simulated["decision"],
                    json!(real_decision),
                    "public {slug} {action} {entity_type:?}: {simulated}"
                );
                compared += 1;

                for who in &people {
                    let token = person_token(&realm, slug, who);
                    let (status, real) = post(
                        &app,
                        &format!("/api/endpoint/{slug}/access/check"),
                        Some(&token),
                        &question(action, entity_type),
                    )
                    .await;
                    let real_decision = status == StatusCode::OK && real["decision"] == json!(true);
                    let mut asked = question(action, entity_type);
                    asked["subject"] = who.clone();
                    let (status, simulated) = simulate(&app, &realm, slug, asked).await;
                    assert_eq!(status, StatusCode::OK, "{simulated}");
                    assert_eq!(
                        simulated["decision"],
                        json!(real_decision),
                        "{who} {slug} {action} {entity_type:?}: real {status} {real}, simulated {simulated}"
                    );
                    // A door that refused the real token is `not_admitted`, never a Policy.
                    if status != StatusCode::OK {
                        assert_eq!(simulated["context"]["reason"], json!("not_admitted"));
                    }
                    compared += 1;
                }
            }
        }
    }
    assert_eq!(compared, 2 * 5 * 3 * 6);
}

/// The answer names the Policy that decided, a refusing prohibition included.
#[tokio::test]
async fn the_answer_names_the_deciding_policy() {
    let realm = common::Realm::new();
    let app = app_of(&realm);
    let cases = [
        (
            json!({}),
            "retrieveEntity",
            Some("AirQualityObserved"),
            true,
            "policy_grant_matched",
            Some("public-air"),
        ),
        (
            json!({}),
            "queryTemporal",
            Some("AirQualityObserved"),
            false,
            "prohibited",
            Some("no-history"),
        ),
        (
            json!({ "roles": ["data-steward"] }),
            "createEntity",
            Some("InternalIncident"),
            true,
            "policy_grant_matched",
            Some("stewards"),
        ),
        (
            json!({ "groups": ["ovzdusie"] }),
            "updateAttrs",
            Some("AirQualityObserved"),
            true,
            "policy_grant_matched",
            Some("ovzdusie-writes"),
        ),
        (
            json!({ "user": "ada" }),
            "deleteEntity",
            Some("AirQualityObserved"),
            true,
            "policy_grant_matched",
            Some("ada-deletes"),
        ),
        // A member of a group with no person of their own: no Policy for one person applies.
        (
            json!({ "groups": ["ovzdusie"] }),
            "deleteEntity",
            Some("AirQualityObserved"),
            false,
            "no_grant",
            None,
        ),
    ];
    for (subject, action, entity_type, decision, reason, policy) in cases {
        let mut asked = question(action, entity_type);
        asked["subject"] = subject.clone();
        let (status, answer) = simulate(&app, &realm, OPEN, asked).await;
        assert_eq!(status, StatusCode::OK, "{answer}");
        assert_eq!(
            answer["decision"],
            json!(decision),
            "{subject} {action}: {answer}"
        );
        assert_eq!(
            answer["context"]["reason"],
            json!(reason),
            "{subject} {action}: {answer}"
        );
        assert_eq!(
            answer["context"].get("policy"),
            policy.map(|p| json!(p)).as_ref(),
            "{subject} {action}: {answer}"
        );
    }
}

/// The public on an organization-only Endpoint never reaches a Policy.
#[tokio::test]
async fn a_subject_the_audience_refuses_is_not_admitted() {
    let realm = common::Realm::new();
    let app = app_of(&realm);
    let mut asked = question("retrieveEntity", Some("AirQualityObserved"));
    asked["subject"] = json!({});
    let (_, answer) = simulate(&app, &realm, CLOSED, asked).await;
    assert_eq!(
        answer,
        json!({ "decision": false, "context": { "reason": "not_admitted" } })
    );
}

/// Only the Portal's own service-account token for the simulate audience is answered.
#[tokio::test]
async fn nobody_but_the_portals_account_may_simulate() {
    let realm = common::Realm::new();
    let app = app_of(&realm);
    let path = format!("/api/endpoint/{OPEN}/access/simulate");
    let mut body = question("retrieveEntity", None);
    body["subject"] = json!({ "user": "ada" });

    let (status, _) = post(&app, &path, None, &body).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "no token");

    // A person signed in through the Portal's own client is not the Portal.
    let person = token_of(&realm, PORTAL_CLIENT, "ada");
    let (status, _) = post(&app, &path, Some(&person), &body).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "a person's token of the Portal's client"
    );

    // Another workload with the right audience.
    let other = token_of(
        &realm,
        "helsinki-pipelines",
        "service-account-helsinki-pipelines",
    );
    let (status, _) = post(&app, &path, Some(&other), &body).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "another client's account");

    // The Portal's account with a data audience: the simulate audience is its own.
    let data = realm.mint(&json!({
        "iss": common::ISSUER,
        "sub": "service-account-portal-api",
        "aud": OPEN,
        "azp": PORTAL_CLIENT,
        "preferred_username": "service-account-portal-api",
        "exp": common::in_seconds(300),
    }));
    let (status, _) = post(&app, &path, Some(&data), &body).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "a token minted for an Endpoint"
    );

    // And the Portal's simulate token is no key to the data paths.
    let (status, _) = post(
        &app,
        &format!("/api/endpoint/{OPEN}/access/check"),
        Some(&portal_token(&realm)),
        &question("retrieveEntity", None),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "the simulate audience on access/check"
    );
}

/// A request that cannot be simulated says why; an unknown Endpoint is not there.
#[tokio::test]
async fn a_malformed_simulation_is_a_bad_request() {
    let realm = common::Realm::new();
    let app = app_of(&realm);
    let bad = [
        json!({ "subject": { "user": "ada", "nickname": "a" }, "action": { "name": "retrieveEntity" } }),
        json!({ "subject": { "user": "ada", "serviceAccount": "ovzdusie-bot" }, "action": { "name": "retrieveEntity" } }),
        json!({ "subject": {}, "action": { "name": "readEverything" } }),
        json!({ "subject": {}, "resource": { "type": "AirQualityObserved" } }),
        json!({ "action": { "name": "retrieveEntity" } }),
    ];
    for body in bad {
        let (status, problem) = simulate(&app, &realm, OPEN, body.clone()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}: {problem}");
        assert!(
            problem["detail"].as_str().is_some_and(|d| !d.is_empty()),
            "{body}: {problem}"
        );
    }
    let (status, _) = simulate(
        &app,
        &realm,
        "zzzzzzzzzzzzzzzzzzzzzzzzzz",
        json!({ "subject": {}, "action": { "name": "retrieveEntity" } }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// One hop as the broker received it: its path, the `x-jc-*` and tenant headers, its body.
type Hop = (String, Vec<(String, String)>, Value);

/// A broker that records every hop and answers each with `status` and `answer`.
async fn recording_broker(status: StatusCode, answer: Value) -> (String, Arc<Mutex<Vec<Hop>>>) {
    use axum::response::IntoResponse;
    let hops: Arc<Mutex<Vec<Hop>>> = Arc::default();
    let recorder = Arc::clone(&hops);
    let app = axum::Router::new().fallback(move |request: Request<Body>| {
        let (recorder, answer) = (Arc::clone(&recorder), answer.clone());
        async move {
            let (parts, body) = request.into_parts();
            let body = axum::body::to_bytes(body, 64 * 1024)
                .await
                .expect("the hop's body");
            let mut pinned: Vec<(String, String)> = parts
                .headers
                .iter()
                .filter(|(name, _)| {
                    name.as_str().starts_with("x-jc-") || name.as_str() == "ngsild-tenant"
                })
                .map(|(name, value)| {
                    (
                        name.as_str().to_owned(),
                        value.to_str().expect("a visible header").to_owned(),
                    )
                })
                .collect();
            pinned.sort();
            recorder.lock().expect("the hop log").push((
                parts.uri.path().to_owned(),
                pinned,
                serde_json::from_slice(&body).unwrap_or(Value::Null),
            ));
            (status, axum::Json(answer)).into_response()
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a free port");
    let url = format!("http://{}", listener.local_addr().expect("an address"));
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (url, hops)
}

/// The two Endpoints served by a broker view each (no other model, no temporal projection).
fn view_app(realm: &common::Realm, upstream: String) -> axum::Router {
    let through_view = |slug: &str, audience| Endpoint {
        view_mapping: None,
        ..endpoint(slug, audience)
    };
    router(Arc::new(
        Gateway::new(
            Broker::new(upstream),
            Box::new(PolicyPdp),
            "banskabystrica.sk",
        )
        .serve([
            through_view(OPEN, Audience::Public),
            through_view(CLOSED, Audience::Organization),
        ])
        .authenticate(
            Arc::new(realm.verifier()),
            ServiceAccounts::new(),
            Some("https://bb.example.sk".to_owned()),
        ),
    ))
}

fn taken(hops: &Mutex<Vec<Hop>>) -> Vec<Hop> {
    std::mem::take(&mut *hops.lock().expect("the hop log"))
}

/// T-3881: through a view the simulator asks the broker's engine, under exactly the headers the
/// subject's own token pins on `access/check`, and answers the engine's decision.
#[tokio::test]
async fn view_the_simulator_asks_the_engine_as_the_subjects_own_token_would() {
    let realm = common::Realm::new();
    let decided = json!({ "decision": true, "context": {
        "reason": "policy_grant_matched", "policy": "ovzdusie/stewards@k4y7pq2mzt6vhx3nbwrs5cjd8f",
        "assigner": "did:web:banskabystrica.sk" } });
    let (upstream, hops) = recording_broker(StatusCode::OK, decided).await;
    let app = view_app(&realm, upstream);
    let asked = question("createEntity", Some("InternalIncident"));
    let subjects = [
        // the public, a role, a group, a person with both, one with a diacritic and a comma
        json!({}),
        json!({ "user": "", "groups": [], "roles": ["data-steward"] }),
        json!({ "user": "", "groups": ["ovzdusie"], "roles": [] }),
        json!({ "user": "stefan", "groups": ["ovzdusie"], "roles": ["data-steward"] }),
        json!({ "user": "Šťastný, Ján", "groups": ["ovzdusie"], "roles": [] }),
    ];
    for who in &subjects {
        let bearer = (who != &json!({})).then(|| person_token(&realm, OPEN, who));
        let (status, _) = post(
            &app,
            &format!("/api/endpoint/{OPEN}/access/check"),
            bearer.as_deref(),
            &asked,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{who}");
        let real = taken(&hops);

        let mut simulation = asked.clone();
        // the Portal names a person's empty user by leaving it out
        simulation["subject"] = match who.as_object() {
            Some(members) => Value::Object(
                members
                    .iter()
                    .filter(|(name, value)| !(*name == "user" && *value == &json!("")))
                    .map(|(name, value)| (name.clone(), value.clone()))
                    .collect(),
            ),
            None => json!({}),
        };
        let (status, answer) = simulate(&app, &realm, OPEN, simulation).await;
        assert_eq!(status, StatusCode::OK, "{who}: {answer}");
        let simulated = taken(&hops);

        assert_eq!(real.len(), 1, "{who}");
        assert_eq!(simulated.len(), 1, "{who}");
        assert_eq!(real[0].0, format!("/x/views/{OPEN}/access/check"));
        assert_eq!(simulated[0].0, format!("/x/views/{OPEN}/access/explain"));
        assert_eq!(simulated[0].1, real[0].1, "{who}: the pinned caller");
        assert_eq!(simulated[0].2, asked, "{who}: the question");
        assert_eq!(
            answer,
            json!({ "decision": true, "context": { "reason": "policy_grant_matched",
                    "policy": "stewards", "assigner": "did:web:banskabystrica.sk" } }),
            "{who}: the Policy under its manifest name"
        );
    }
}

/// The engine's refusals come back with the Policy that ended them, under its manifest name; an
/// id the Portal did not write is left out rather than shown.
#[tokio::test]
async fn view_the_engines_reasons_name_the_policy_as_the_portal_lists_it() {
    let realm = common::Realm::new();
    for (engine, shown) in [
        (
            json!({ "decision": false, "context": { "reason": "prohibited",
                    "policy": "ovzdusie/no-history", "assigner": "did:web:banskabystrica.sk" } }),
            json!({ "decision": false, "context": { "reason": "prohibited",
                    "policy": "no-history", "assigner": "did:web:banskabystrica.sk" } }),
        ),
        (
            json!({ "decision": false, "context": { "reason": "no_grant" } }),
            json!({ "decision": false, "context": { "reason": "no_grant" } }),
        ),
        (
            json!({ "decision": true, "context": { "reason": "policy_grant_matched", "policy": "loose" } }),
            json!({ "decision": true, "context": { "reason": "policy_grant_matched" } }),
        ),
        (
            json!({ "decision": true, "context": { "reason": "policy_grant_matched", "policy": "a/b/c" } }),
            json!({ "decision": true, "context": { "reason": "policy_grant_matched" } }),
        ),
        (
            json!({ "decision": true, "context": { "reason": "policy_grant_matched", "policy": "ovzdusie/@x" } }),
            json!({ "decision": true, "context": { "reason": "policy_grant_matched" } }),
        ),
    ] {
        let (upstream, _) = recording_broker(StatusCode::OK, engine.clone()).await;
        let app = view_app(&realm, upstream);
        let mut asked = question("queryTemporal", Some("AirQualityObserved"));
        asked["subject"] = json!({});
        let (status, answer) = simulate(&app, &realm, OPEN, asked).await;
        assert_eq!(status, StatusCode::OK, "{engine}");
        assert_eq!(answer, shown, "{engine}");
    }
}

/// The audience refuses before the engine is asked; a broker that does not know the view, or
/// fails, is the answer the caller gets, never a made-up decision.
#[tokio::test]
async fn view_a_refusal_at_the_door_or_by_the_broker_is_never_a_decision() {
    let realm = common::Realm::new();
    let (upstream, hops) = recording_broker(StatusCode::OK, json!({ "decision": true })).await;
    let app = view_app(&realm, upstream);
    let mut asked = question("retrieveEntity", Some("AirQualityObserved"));
    asked["subject"] = json!({});
    let (_, answer) = simulate(&app, &realm, CLOSED, asked.clone()).await;
    assert_eq!(
        answer,
        json!({ "decision": false, "context": { "reason": "not_admitted" } })
    );
    assert!(taken(&hops).is_empty(), "the engine is not asked");

    for status in [StatusCode::NOT_FOUND, StatusCode::INTERNAL_SERVER_ERROR] {
        let (upstream, _) = recording_broker(status, json!({ "title": "no such view" })).await;
        let app = view_app(&realm, upstream);
        let (got, answer) = simulate(&app, &realm, OPEN, asked.clone()).await;
        assert_ne!(got, StatusCode::OK, "{status}: {answer}");
        assert!(answer.get("decision").is_none(), "{status}: {answer}");
    }
}

/// T-3792: with no action the answer is everything the subject is granted, the `access`
/// document a token of theirs would read, beside the Policies in force for them; a subject the
/// audience refuses is not admitted, and the document names nothing of what they cannot reach.
#[tokio::test]
async fn with_no_action_the_answer_is_the_subjects_whole_grant() {
    let realm = common::Realm::new();
    let app = app_of(&realm);

    let ada = json!({ "subject": { "user": "ada", "groups": ["ovzdusie"] } });
    let (status, answer) = simulate(&app, &realm, CLOSED, ada).await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(answer["admitted"], json!(true));
    assert_eq!(answer["resource"]["id"], json!(CLOSED));
    assert_eq!(
        answer["policies"],
        json!(["ovzdusie-writes", "ada-deletes"])
    );
    let actions: Vec<&str> = answer["permissions"]
        .as_array()
        .expect("permissions")
        .iter()
        .flat_map(|entry| entry["actions"].as_array().expect("actions"))
        .filter_map(Value::as_str)
        .collect();
    assert_eq!(actions, ["updateAttrs", "deleteEntity"], "{answer}");
    assert!(
        !answer.to_string().contains("InternalIncident"),
        "EP-59: {answer}"
    );

    let (_, public) = simulate(&app, &realm, OPEN, json!({ "subject": {} })).await;
    assert_eq!(public["policies"], json!(["public-air", "no-history"]));

    let (_, refused) = simulate(&app, &realm, CLOSED, json!({ "subject": {} })).await;
    assert_eq!(refused, json!({ "admitted": false }));
}
