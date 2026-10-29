//! T-5624: the requests an Endpoint's rate limit refuses reach the project's Activity as one
//! `endpoint.rate-limited` row per caller with their count, naming a ServiceAccount by its
//! name and a caller without a token as `anonymous`, never by its address.
//!
//! Alone in its binary: the activity tally is the process's own.

#[path = "../common/mod.rs"]
mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use context_gateway::activity;
use context_gateway::app::{router, Gateway};
use context_gateway::auth::accounts::{accounts_of, client_id};
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::Endpoint;
use jc_core::kinds::{Audience, RateLimits, Representation};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::Arc;
use tower::ServiceExt;

const SLUG: &str = "cccccccccccccccccccccccccc";
const ADDRESS: &str = "203.0.113.77";

fn endpoint() -> Endpoint {
    Endpoint {
        declared_types: None,
        roles: Default::default(),
        slug: SLUG.to_owned(),
        title: Default::default(),
        description: Default::default(),
        space: "walk-ops-walk".to_owned(),
        project: "walk-ops".to_owned(),
        audience: Audience::Public,
        allowed_projects: Vec::new(),
        representations: vec![Representation::NgsiLd],
        rate_limit: Some(RateLimits {
            requests_per_minute: 1,
            burst: Some(1),
        }),
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
        base_path: format!("/api/endpoint/{SLUG}"),
        models: Vec::new(),
        policy_names: Vec::new(),
        manifest_name: String::new(),
        policies: Vec::new(),
    }
}

/// A repository holding the ServiceAccount walk-caller of project walk-ops.
fn accounts() -> context_gateway::auth::accounts::ServiceAccounts {
    let dir = std::env::temp_dir().join(format!("gateway-rate-activity-{}", std::process::id()));
    let space = dir.join("projects/walk-ops/spaces/walk");
    let account = dir.join("projects/walk-ops/access/serviceaccounts");
    std::fs::create_dir_all(&space).expect("the space directory");
    std::fs::create_dir_all(&account).expect("the account directory");
    std::fs::write(
        space.join("space.yaml"),
        "apiVersion: joinedcontext.com/v1alpha1\nkind: ContextSpace\nmetadata:\n  name: walk\n  namespace: walk-ops\nspec:\n  urnSegment: walk-ops-walk\n",
    )
    .expect("the space is written");
    std::fs::write(
        account.join("walk-caller.yaml"),
        "apiVersion: joinedcontext.com/v1alpha1\nkind: ServiceAccount\nmetadata:\n  name: walk-caller\n  namespace: walk-ops\nspec:\n  owner:\n    user: demo.steward\n  purpose: \"drill B\"\n  roles: []\n  credentials:\n    - kind: oauth-client\n      name: default\n",
    )
    .expect("the account is written");
    let loaded = jc_manifest::loader::Repository::load(&dir).expect("the repository loads");
    let _ = std::fs::remove_dir_all(&dir);
    let accounts = accounts_of(&loaded);
    assert_eq!(accounts.len(), 1, "the account is in the table");
    accounts
}

fn get(token: Option<&str>) -> Request<Body> {
    let mut request = Request::get(format!("/api/endpoint/{SLUG}/ngsi-ld/v1/entities?type=T"))
        .header("x-forwarded-for", ADDRESS);
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    request.body(Body::empty()).expect("a request")
}

fn named(record: &Value) -> BTreeMap<String, String> {
    let mut named = BTreeMap::new();
    for attribute in record["attributes"].as_array().into_iter().flatten() {
        let value = &attribute["value"];
        let text = value
            .get("stringValue")
            .or_else(|| value.get("intValue"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        named.insert(
            attribute["key"].as_str().unwrap_or_default().to_owned(),
            text.to_owned(),
        );
    }
    named
}

#[tokio::test]
async fn a_flood_is_one_row_per_caller_naming_the_account_and_never_an_address() {
    let realm = common::Realm::new();
    let broker = common::BrokerStub::start(vec![json!([]); 4]).await;
    let gateway = Arc::new(
        Gateway::new(
            Broker::new(&broker.url),
            Box::new(PolicyPdp),
            "banskabystrica.sk",
        )
        .serve([endpoint()])
        .authenticate(Arc::new(realm.verifier()), accounts(), None),
    );
    activity::name_spaces(&BTreeMap::from([(
        "walk-ops-walk".to_owned(),
        "walk".to_owned(),
    )]));
    let app = router(gateway);
    let azp = client_id("walk-ops", "walk-caller");
    let token = realm.mint(&json!({
        "iss": common::ISSUER,
        "sub": "5b1e-walk",
        "aud": SLUG,
        "azp": azp,
        "preferred_username": format!("service-account-{azp}"),
        "exp": common::in_seconds(300),
        "iat": common::in_seconds(-10),
    }));

    let mut refused = 0;
    for _ in 0..6 {
        let status = app
            .clone()
            .oneshot(get(Some(&token)))
            .await
            .expect("the gateway answers")
            .status();
        if status == StatusCode::TOO_MANY_REQUESTS {
            refused += 1;
        }
    }
    assert_eq!(refused, 5, "the burst of one lets the first call through");
    for _ in 0..3 {
        let status = app
            .clone()
            .oneshot(get(None))
            .await
            .expect("the gateway answers")
            .status();
        assert_ne!(status, StatusCode::INTERNAL_SERVER_ERROR);
    }

    let rows = activity::take(true);
    let records: Vec<BTreeMap<String, String>> = rows
        .iter()
        .map(|(call, held)| named(&activity::log_record(call, held)))
        .collect();
    let text = format!("{records:?}");
    for absent in [ADDRESS, token.as_str(), "5b1e-walk"] {
        assert!(
            !text.contains(absent),
            "{absent} reached the Activity: {text}"
        );
    }
    let limited: Vec<&BTreeMap<String, String>> = records
        .iter()
        .filter(|row| row["kind"] == "endpoint.rate-limited")
        .collect();
    assert_eq!(limited.len(), 2, "one row per caller: {text}");
    assert_eq!(limited[0]["project"], "walk-ops");
    assert_eq!(limited[0]["space"], "walk");
    assert_eq!(limited[0]["endpoint"], SLUG);
    assert_eq!(limited[0]["count"], "2");
    assert_eq!(
        limited[0]["summary"],
        format!("anonymous was refused 2 times on {SLUG}: its rate limit was spent")
    );
    assert_eq!(limited[1]["count"], "5");
    assert_eq!(
        limited[1]["summary"],
        format!(
            "serviceAccount:walk-caller was refused 5 times on {SLUG}: its rate limit was spent"
        )
    );
}
