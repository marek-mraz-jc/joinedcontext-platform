//! A personal slot reaches the broker minimised (ADR-N-054 2.6, DM-121, T-4666): one the model
//! marks `keyedHash` as the HMAC-SHA-256 of its value under the space's key, one it marks
//! `drop` not at all. Without the space's key the write is refused, never stored as sent.

mod common;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use context_gateway::app::{router, Gateway};
use context_gateway::auth::accounts::ServiceAccounts;
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::{DeclaredTypes, Endpoint};
use jc_core::kinds::{Audience, PolicySpec, Representation};
use jc_manifest::model_schema::ModelSchema;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;
use tower::ServiceExt;

const SLUG: &str = "pppppppppppppppppppppppppp";
const RIDER: &str = "rider-jane-doe";

fn schema() -> Arc<ModelSchema> {
    let model = json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "definitions": {
            "BikeTrip": {
                "type": "object",
                "properties": {
                    "id": { "type": "string" },
                    "rider": { "type": "string", "x-personal-data": true, "x-ingest": "keyedHash" },
                    "location": { "type": "object", "x-personal-data": true, "x-ingest": "drop" },
                    "startStation": { "type": "string" }
                }
            }
        }
    });
    Arc::new(ModelSchema::compile(
        "bike-trips",
        "1.0.0",
        &model,
        &["BikeTrip".to_owned()],
        true,
    ))
}

fn endpoint() -> Endpoint {
    let slug = SLUG;
    let hidden: &[&str] = &[];
    let grant: PolicySpec = serde_norway::from_str(
        "contextSpaceRef: trips\nassigner: did:web:banskabystrica.sk\n\
         assignee: { kind: role, id: steward }\noperations: [createEntity]\n",
    )
    .expect("the policy spec parses");
    Endpoint {
        declared_types: Some(Arc::new(DeclaredTypes {
            model: "bike-trips".into(),
            classes: ["BikeTrip".to_owned()].into(),
            units: Default::default(),
            relationships: Default::default(),
            quality: Vec::new(),
            schema: Some(schema()),
        })),
        roles: Default::default(),
        slug: slug.to_owned(),
        title: Default::default(),
        description: Default::default(),
        space: "trips".to_owned(),
        project: "trips".to_owned(),
        audience: Audience::Organization,
        allowed_projects: Vec::new(),
        representations: vec![Representation::NgsiLd],
        rate_limit: None,
        creates: None,
        file_limits: None,
        hidden_attributes: hidden.iter().map(|h| (*h).to_owned()).collect(),
        projection: None,
        view_mapping: None,
        catalog: None,
        deprecation: None,
        feeds: Vec::new(),
        calendars: Vec::new(),
        map_defaults: Default::default(),
        base_path: format!("/api/endpoint/{slug}"),
        models: Vec::new(),
        policy_names: Vec::new(),
        manifest_name: String::new(),
        policies: vec![grant],
    }
}

async fn write(keys: Option<PathBuf>) -> (StatusCode, Value, Vec<common::Hop>) {
    let broker = common::BrokerStub::start(vec![json!([])]).await;
    let realm = common::Realm::new();
    let gateway = Gateway::new(
        Broker::new(&broker.url),
        Box::new(PolicyPdp),
        "banskabystrica.sk",
    )
    .serve(vec![endpoint()])
    .authenticate(
        Arc::new(realm.verifier()),
        ServiceAccounts::new(),
        Some("https://city.example".to_owned()),
    );
    let gateway = match keys {
        Some(dir) => gateway.personal_keys_in(dir),
        None => gateway,
    };
    let token = realm.mint(&json!({
        "iss": common::ISSUER,
        "sub": "0f5a",
        "aud": "context-gateway",
        "preferred_username": "jana",
        "groups": ["/trips"],
        "realm_access": { "roles": ["steward"] },
        "exp": common::in_seconds(300),
        "iat": common::in_seconds(-10),
    }));
    let trip = json!({
        "id": "urn:ngsi-ld:BikeTrip:t1", "type": "BikeTrip",
        "rider": { "type": "Property", "value": RIDER },
        "location": { "type": "GeoProperty", "value": { "type": "Point", "coordinates": [19.15, 48.74] } },
        "startStation": { "type": "Property", "value": "Namestie SNP" }
    });
    let response = router(Arc::new(gateway))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/endpoint/{SLUG}/ngsi-ld/v1/entities"))
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(trip.to_string()))
                .expect("a request"),
        )
        .await
        .expect("the gateway answers");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("a body");
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        broker.hops(),
    )
}

fn keys(name: &str, key: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("jc-personal-keys-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("a key directory");
    std::fs::write(dir.join("trips"), key).expect("the space's key");
    dir
}

#[tokio::test]
async fn the_broker_keeps_the_rider_hashed_and_no_location() {
    let dir = keys("ok", "Lq2-a-space-personal-data-key-of-32-bytes\n");
    let (status, answer, hops) = write(Some(dir.clone())).await;
    assert!(status.is_success(), "{status} {answer}");
    let posted: Vec<_> = hops.iter().filter(|hop| !hop.body.is_empty()).collect();
    assert_eq!(posted.len(), 1, "one write reaches the broker");
    let stored: Value = serde_json::from_slice(&posted[0].body).expect("a JSON body");
    let rider = stored["rider"]["value"].as_str().unwrap_or_default();
    assert_eq!(rider.len(), 64, "{stored}");
    assert!(rider.bytes().all(|b| b.is_ascii_hexdigit()));
    assert!(stored.get("location").is_none(), "{stored}");
    assert_eq!(stored["startStation"]["value"], "Namestie SNP");
    assert!(!String::from_utf8_lossy(&posted[0].body).contains(RIDER));

    // Equal values stay linkable: the same rider under the same key is the same hash.
    let (_, _, again) = write(Some(dir.clone())).await;
    let again: Value = serde_json::from_slice(
        &again
            .iter()
            .find(|hop| !hop.body.is_empty())
            .expect("a write")
            .body,
    )
    .expect("a JSON body");
    assert_eq!(again["rider"]["value"], stored["rider"]["value"]);
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn without_the_spaces_key_nothing_reaches_the_broker() {
    let short = keys("short", "too-short");
    for dir in [None, Some(short.clone())] {
        let (status, problem, hops) = write(dir).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{problem}");
        assert!(problem.to_string().contains("rider"), "{problem}");
        assert!(!problem.to_string().contains(RIDER), "{problem}");
        assert!(
            hops.iter().all(|hop| hop.body.is_empty()),
            "nothing was stored"
        );
    }
    let _ = std::fs::remove_dir_all(short);
}
