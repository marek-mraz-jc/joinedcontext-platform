//! T-6186: the usage counts name the kind of caller from the token the realm signed. A token the
//! realm did not sign names nobody, so whatever it is answered is never counted as a person.

use crate::common;

use axum::body::Body;
use axum::http::{header, Request};
use context_gateway::app::{router, Gateway};
use context_gateway::auth::accounts::ServiceAccounts;
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::Endpoint;
use context_gateway::usage;
use jc_core::kinds::{Audience, PolicySpec, Representation};
use serde_json::json;
use std::sync::Arc;
use tower::ServiceExt;

/// A slug no other test serves: the tally is the process's, so this test reads only its own lines.
const SLUG: &str = "u6r2m4xq9vbn3tdw6hcy5pajfe";
const SPACE: &str = "ovzdusie";

fn endpoint() -> Endpoint {
    let policy: PolicySpec = serde_norway::from_str(&format!(
        "contextSpaceRef: {SPACE}\nassigner: did:web:example.org\n\
         assignee: {{ kind: role, id: public }}\noperations: [queryEntity]\n\
         information:\n  - entities:\n      - type: AirQualityObserved\n"
    ))
    .expect("the policy spec parses");
    Endpoint {
        declared_types: None,
        catalog: None,
        deprecation: None,
        feeds: Vec::new(),
        calendars: Vec::new(),
        map_defaults: Default::default(),
        roles: Default::default(),
        slug: SLUG.to_owned(),
        title: Default::default(),
        description: Default::default(),
        space: SPACE.to_owned(),
        project: SPACE.to_owned(),
        audience: Audience::Public,
        allowed_projects: Vec::new(),
        representations: vec![Representation::NgsiLd],
        rate_limit: None,
        creates: None,
        file_limits: None,
        hidden_attributes: Default::default(),
        projection: None,
        view_mapping: None,
        base_path: format!("/api/endpoint/{SLUG}"),
        models: Vec::new(),
        policy_names: Vec::new(),
        manifest_name: String::new(),
        policies: vec![policy],
    }
}

fn read(token: Option<&str>) -> Request<Body> {
    let mut request = Request::builder()
        .uri(format!(
            "/api/endpoint/{SLUG}/ngsi-ld/v1/entities?type=AirQualityObserved"
        ))
        .header("x-forwarded-for", "192.0.2.7");
    if let Some(token) = token {
        request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    request.body(Body::empty()).expect("a request")
}

fn person(realm: &common::Realm) -> String {
    realm.mint(&json!({
        "iss": common::ISSUER,
        "sub": "0b6f2c1e-person",
        "aud": SLUG,
        "azp": "portal-ui",
        "preferred_username": "ana.steward",
        "exp": common::in_seconds(300),
        "iat": common::in_seconds(-10),
    }))
}

#[tokio::test]
async fn a_count_names_the_verified_callers_kind_and_a_forged_token_is_no_person() {
    let broker = common::BrokerStub::start(vec![json!([])]).await;
    let realm = common::Realm::new();
    let app = router(Arc::new(
        Gateway::new(
            Broker::new(broker.url.clone()),
            Box::new(PolicyPdp),
            "example.org",
        )
        .serve([endpoint()])
        .authenticate(Arc::new(realm.verifier()), ServiceAccounts::default(), None),
    ));
    let mut statuses = Vec::new();
    let signed = person(&realm);
    let forged = person(&common::Realm::new());
    for token in [None, Some(signed.as_str()), Some(forged.as_str())] {
        let answer = app.clone().oneshot(read(token)).await.expect("an answer");
        statuses.push(answer.status().as_u16());
    }
    let mine: Vec<usage::Count> = usage::take()
        .into_iter()
        .filter(|c| c.endpoint == SLUG)
        .collect();
    let calls = |caller: &str| -> u64 {
        mine.iter()
            .filter(|c| c.caller == caller)
            .map(|c| c.count)
            .sum()
    };
    assert_eq!(statuses[..2], [200, 200], "{statuses:?}");
    assert_eq!(calls("person"), 1, "{mine:?}");
    // The forged token is refused or served as nobody; it never adds a person.
    let forged_counted = u64::from(statuses[2] < 400);
    assert_eq!(
        calls("anonymous"),
        1 + forged_counted,
        "{mine:?} {statuses:?}"
    );
    assert_eq!(calls("workload") + calls("agent"), 0, "{mine:?}");
}
