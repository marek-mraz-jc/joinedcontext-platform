//! T-3883: the subscriptions stored with an R12 endpoint move to their view, under their own
//! subscriber and target, and what cannot be read stays as it is stored.

use axum::extract::{Request, State};
use axum::http::{Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::{Json, Router};
use context_gateway::egress::migrate::{self, Outcome, Surface};
use context_gateway::egress::subject::DeliveryKey;
use context_gateway::pdp::evaluator::Subject;
use context_gateway::proxy::Broker;
use context_gateway::query::encode;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

const SLUG: &str = "n4t8xq2vhm6zc9wrb5sdj3kfp7";
const SPACE: &str = "ovzdusie";
const PORTAL: &str = "joinedcontext-portal";
/// The view whose create this stub refuses, to see the original put back.
const REFUSING_VIEW_ID: &str = "urn:ngsi-ld:Subscription:ovzdusie:refused";

/// What the broker stub holds and what it was asked.
#[derive(Default)]
struct Held {
    /// The plain surface's subscriptions of [`SPACE`].
    plain: Vec<Value>,
    /// What was created under a view: the path, the `x-jc-subject` header, the body.
    viewed: Vec<(String, Option<String>, Value)>,
}

type Stub = Arc<Mutex<Held>>;

async fn broker(State(held): State<Stub>, request: Request) -> Response {
    let (method, path) = (request.method().clone(), request.uri().path().to_owned());
    let tenant = request
        .headers()
        .get("ngsild-tenant")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let subject = request
        .headers()
        .get("x-jc-subject")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let bytes = axum::body::to_bytes(request.into_body(), 1 << 20)
        .await
        .unwrap_or_default();
    let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    if tenant.as_deref() != Some(SPACE) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let mut held = held.lock().expect("stub");
    match (method, path.as_str()) {
        (Method::GET, "/ngsi-ld/v1/subscriptions") => Json(held.plain.clone()).into_response(),
        (Method::DELETE, by_id) => {
            let id = context_gateway::query::decode(
                by_id
                    .strip_prefix("/ngsi-ld/v1/subscriptions/")
                    .unwrap_or_default(),
            );
            let before = held.plain.len();
            held.plain.retain(|s| s["id"] != id.as_str());
            match held.plain.len() < before {
                true => StatusCode::NO_CONTENT.into_response(),
                false => StatusCode::NOT_FOUND.into_response(),
            }
        }
        (Method::POST, "/ngsi-ld/v1/subscriptions") => {
            held.plain.push(body);
            StatusCode::CREATED.into_response()
        }
        (Method::POST, view) if view.starts_with("/x/views/") => {
            if body["id"] == REFUSING_VIEW_ID {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"detail": "watched attributes outside the view"})),
                )
                    .into_response();
            }
            held.viewed.push((view.to_owned(), subject, body));
            StatusCode::CREATED.into_response()
        }
        _ => StatusCode::METHOD_NOT_ALLOWED.into_response(),
    }
}

async fn start(plain: Vec<Value>) -> (Broker, Stub) {
    let held: Stub = Arc::new(Mutex::new(Held {
        plain,
        ..Held::default()
    }));
    let app = Router::new()
        .fallback(any(broker))
        .with_state(Arc::clone(&held));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a port");
    let address = listener.local_addr().expect("an address");
    tokio::spawn(async move { axum::serve(listener, app).await });
    (Broker::new(format!("http://{address}")), held)
}

fn key() -> DeliveryKey {
    DeliveryKey::new(&[3u8; 32]).expect("32 bytes")
}

fn surfaces() -> Vec<Surface> {
    vec![Surface {
        space: SPACE.to_owned(),
        base_path: format!("/api/endpoint/{SLUG}"),
        view: SLUG.to_owned(),
    }]
}

fn user(name: &str) -> Subject {
    Subject {
        user: Some(name.to_owned()),
        ..Subject::default()
    }
}

/// A subscription as T-2383 stored it: routed back through R12, the subscriber sealed.
fn routed(id: &str, target: &str, subject: &Subject, key: &DeliveryKey) -> Value {
    let base = format!("/api/endpoint/{SLUG}");
    let sealed = key.seal(&base, target, &[], subject);
    json!({
        "id": id,
        "type": "Subscription",
        "entities": [{"type": "AirQualityObserved"}],
        "createdAt": "2026-10-01T00:00:00Z",
        "notification": {
            "attributes": ["pm10"],
            "timesSent": 2,
            "endpoint": {"uri": format!(
                "http://context-gateway:8080{base}/egress/notifications?to={}&sub={}",
                encode(target),
                encode(&sealed)
            )}
        }
    })
}

fn fixture() -> Vec<Value> {
    let other = DeliveryKey::new(&[4u8; 32]).expect("32 bytes");
    let portal = Subject {
        service_account: Some(PORTAL.to_owned()),
        ..Subject::default()
    };
    vec![
        routed(
            "urn:ngsi-ld:Subscription:ovzdusie:alice",
            "https://hooks.example.com/a",
            &user("alice"),
            &key(),
        ),
        routed(
            "urn:ngsi-ld:Subscription:ovzdusie:portal",
            "https://hooks.example.com/p",
            &portal,
            &key(),
        ),
        routed(
            "urn:ngsi-ld:Subscription:ovzdusie:forged",
            "https://hooks.example.com/f",
            &user("mallory"),
            &other,
        ),
        routed(
            "urn:ngsi-ld:Subscription:ovzdusie:inward",
            "http://10.0.0.7/in",
            &user("bob"),
            &key(),
        ),
        routed(
            REFUSING_VIEW_ID,
            "https://hooks.example.com/r",
            &user("carol"),
            &key(),
        ),
        json!({
            "id": "urn:ngsi-ld:Subscription:ovzdusie:direct",
            "notification": {"endpoint": {"uri": "https://hooks.example.com/d"}}
        }),
    ]
}

#[tokio::test]
async fn the_dry_run_counts_each_kind_and_changes_nothing() {
    let (broker, held) = start(fixture()).await;
    let survey = migrate::survey(&broker, &surfaces(), &key(), PORTAL, &[])
        .await
        .expect("a survey");
    let counts = survey.counts();
    let counts = counts.get(SPACE).expect("the space");
    assert_eq!(
        (
            counts.routed,
            counts.reconciler,
            counts.rewritable,
            counts.unreadable,
            counts.refused
        ),
        (5, 1, 2, 1, 1)
    );
    assert_eq!(
        survey.stored().len(),
        5,
        "the direct one is no R12 subscription"
    );
    let held = held.lock().expect("stub");
    assert_eq!(held.plain.len(), 6);
    assert!(held.viewed.is_empty());
}

#[tokio::test]
async fn a_real_run_moves_what_opens_and_puts_back_what_the_view_refuses() {
    let (broker, held) = start(fixture()).await;
    let survey = migrate::survey(&broker, &surfaces(), &key(), PORTAL, &[])
        .await
        .expect("a survey");
    let outcomes = migrate::apply(&broker, &survey).await;
    assert_eq!(outcomes.len(), 2);
    assert!(outcomes.contains(&(
        "urn:ngsi-ld:Subscription:ovzdusie:alice".to_owned(),
        Outcome::Rewritten
    )));
    let refused = outcomes
        .iter()
        .find(|(id, _)| id == REFUSING_VIEW_ID)
        .map(|(_, outcome)| outcome);
    assert!(
        matches!(refused, Some(Outcome::Failed(why)) if why.contains("restored")),
        "{refused:?}"
    );

    let held = held.lock().expect("stub");
    let [(path, subject, body)] = held.viewed.as_slice() else {
        panic!("one move: {:?}", held.viewed);
    };
    assert_eq!(path, &format!("/x/views/{SLUG}/ngsi-ld/v1/subscriptions"));
    assert_eq!(subject.as_deref(), Some("alice"));
    assert_eq!(body["id"], "urn:ngsi-ld:Subscription:ovzdusie:alice");
    assert_eq!(
        body["notification"]["endpoint"]["uri"],
        "https://hooks.example.com/a"
    );
    assert!(body.get("createdAt").is_none());
    assert!(body["notification"].get("timesSent").is_none());

    let ids: Vec<&str> = held.plain.iter().filter_map(|s| s["id"].as_str()).collect();
    assert!(!ids.contains(&"urn:ngsi-ld:Subscription:ovzdusie:alice"));
    for left in ["portal", "forged", "inward", "refused", "direct"] {
        let id = format!("urn:ngsi-ld:Subscription:ovzdusie:{left}");
        assert!(ids.contains(&id.as_str()), "{left} is still stored");
    }
    // The restored one routes as it did, the seal intact.
    let restored = held
        .plain
        .iter()
        .find(|s| s["id"] == REFUSING_VIEW_ID)
        .expect("restored");
    let uri = restored["notification"]["endpoint"]["uri"]
        .as_str()
        .unwrap_or_default();
    assert!(uri.contains("/egress/notifications?to=") && uri.contains("&sub="));
}

#[tokio::test]
async fn a_space_the_broker_never_saw_has_nothing_to_move() {
    let (broker, _) = start(Vec::new()).await;
    let mut surfaces = surfaces();
    surfaces[0].space = "prazdny".to_owned();
    let survey = migrate::survey(&broker, &surfaces, &key(), PORTAL, &[])
        .await
        .expect("a survey");
    assert!(survey.stored().is_empty());
    assert!(matches!(
        survey.spaces.get("prazdny").map(Vec::as_slice),
        Some([])
    ));
}
