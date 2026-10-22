//! A deprecated Endpoint says so on every answer that carries data, and on no refusal (EP-147).
//!
//! `Deprecation` (RFC 9745) and `Sunset` (RFC 8594) come from `spec.deprecation`; the values are
//! the two dates and the author's public link, so a consumer learns when to move and nothing
//! about the space behind the Endpoint.

mod common;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use axum::routing::any;
use axum::Router;
use context_gateway::app::{router, Gateway};
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::Endpoint;
use jc_core::kinds::endpoint_deprecation::Deprecation;
use jc_core::kinds::{Audience, PolicySpec, Representation};
use serde_json::json;
use std::sync::Arc;
use tower::ServiceExt;

const OPEN: &str = "k4y7pq2mzt6vhx3nbwrs5cjd8f";
const SURFACE: &str = "m3q5tz7wxk2pd4vbnh6rcsj8ya";
const CLOSED: &str = "p2r6tz4wxk7md3vbnh5qcsj8yb";
const PLAIN: &str = "q7r2tz4wxk6md3vbnh5pcsj8yc";
const SPACE: &str = "ovzdusie";
const LINK: &str = "https://example.org/open-data/air-quality-v2";

async fn broker() -> String {
    let app = Router::new().fallback(any(|| async {
        axum::Json(json!([{
            "id": "urn:ngsi-ld:AirQualityObserved:station-01",
            "type": "AirQualityObserved",
            "pm10": { "type": "Property", "value": 34.2 },
            "location": {
                "type": "GeoProperty",
                "value": { "type": "Point", "coordinates": [19.15, 48.73] },
            },
        }]))
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a free port");
    let address = listener.local_addr().expect("an address");
    tokio::spawn(async move {
        let _ = axum::serve(listener, common::plain_tree(app)).await;
    });
    format!("http://{address}")
}

fn deprecation(representations: Vec<Representation>, link: Option<&str>) -> Deprecation {
    Deprecation {
        since: "2026-11-02".parse().expect("a date"),
        sunset: "2027-02-01".parse().expect("a date"),
        link: link.map(str::to_owned),
        representations,
        attributes: Vec::new(),
    }
}

fn endpoint(slug: &str, audience: Audience, deprecation: Option<Deprecation>) -> Endpoint {
    let policy: PolicySpec = serde_norway::from_str(
        "contextSpaceRef: ovzdusie\n\
         assigner: did:web:bb.sk\n\
         assignee: { kind: role, id: public }\n\
         operations: [queryEntity, retrieveEntity]\n\
         information:\n  - entities:\n      - type: AirQualityObserved\n",
    )
    .expect("the policy spec parses");
    Endpoint {
        declared_types: None,
        roles: Default::default(),
        slug: slug.to_owned(),
        title: Default::default(),
        description: Default::default(),
        space: SPACE.to_owned(),
        project: SPACE.to_owned(),
        audience,
        allowed_projects: Vec::new(),
        representations: vec![
            Representation::NgsiLd,
            Representation::Csv,
            Representation::OgcFeatures,
        ],
        rate_limit: None,
        creates: None,
        file_limits: None,
        hidden_attributes: Default::default(),
        projection: None,
        view_mapping: None,
        catalog: None,
        deprecation: deprecation.map(Arc::new),
        feeds: Vec::new(),
        calendars: Vec::new(),
        base_path: format!("/api/endpoint/{slug}"),
        models: Vec::new(),
        policy_names: Vec::new(),
        manifest_name: String::new(),
        policies: vec![policy],
    }
}

/// The status and every `Deprecation`, `Sunset` and `Link` header of one `GET`.
async fn headers_of(uri: &str) -> (StatusCode, Vec<String>, Vec<String>, Vec<String>) {
    let gateway = Gateway::new(Broker::new(broker().await), Box::new(PolicyPdp), "bb.sk").serve([
        endpoint(
            OPEN,
            Audience::Public,
            Some(deprecation(Vec::new(), Some(LINK))),
        ),
        endpoint(
            SURFACE,
            Audience::Public,
            Some(deprecation(vec![Representation::OgcFeatures], None)),
        ),
        endpoint(
            CLOSED,
            Audience::Organization,
            Some(deprecation(Vec::new(), Some(LINK))),
        ),
        endpoint(PLAIN, Audience::Public, None),
    ]);
    let response = router(Arc::new(gateway))
        .oneshot(
            Request::builder()
                .uri(uri)
                .body(Body::empty())
                .expect("a request"),
        )
        .await
        .expect("the gateway answers");
    let all = |name: &str| -> Vec<String> {
        response
            .headers()
            .get_all(name)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .map(str::to_owned)
            .collect()
    };
    let links = all(header::LINK.as_str())
        .into_iter()
        .filter(|link| link.contains("rel=\"deprecation\""))
        .collect();
    (response.status(), all("deprecation"), all("sunset"), links)
}

#[tokio::test]
async fn a_deprecated_endpoint_announces_both_dates_and_its_link_on_every_surface() {
    for uri in [
        format!("/api/endpoint/{OPEN}/ngsi-ld/v1/entities?type=AirQualityObserved"),
        format!("/api/endpoint/{OPEN}/file.csv"),
        format!("/api/endpoint/{OPEN}/ogc/features/collections/AirQualityObserved/items"),
    ] {
        let (status, deprecation, sunset, links) = headers_of(&uri).await;
        assert!(status.is_success(), "{uri} answered {status}");
        assert_eq!(deprecation, ["@1793577600"], "{uri}");
        assert_eq!(sunset, ["Mon, 01 Feb 2027 00:00:00 GMT"], "{uri}");
        assert_eq!(
            links,
            [format!("<{LINK}>; rel=\"deprecation\"; type=\"text/html\"")],
            "{uri}"
        );
    }
}

#[tokio::test]
async fn a_surface_deprecation_is_announced_on_that_surface_alone() {
    let ogc = format!("/api/endpoint/{SURFACE}/ogc/features/collections/AirQualityObserved/items");
    let (status, deprecation, sunset, links) = headers_of(&ogc).await;
    assert!(status.is_success(), "{status}");
    assert_eq!(deprecation, ["@1793577600"]);
    assert_eq!(sunset, ["Mon, 01 Feb 2027 00:00:00 GMT"]);
    assert!(links.is_empty(), "no link was set: {links:?}");

    let ngsi = format!("/api/endpoint/{SURFACE}/ngsi-ld/v1/entities?type=AirQualityObserved");
    let (status, deprecation, sunset, _) = headers_of(&ngsi).await;
    assert!(status.is_success(), "{status}");
    assert!(
        deprecation.is_empty() && sunset.is_empty(),
        "{deprecation:?} {sunset:?}"
    );
}

#[tokio::test]
async fn a_refusal_and_an_endpoint_without_deprecation_carry_none() {
    for uri in [
        // Refused: no token for an organization Endpoint.
        format!("/api/endpoint/{CLOSED}/ngsi-ld/v1/entities?type=AirQualityObserved"),
        // Not found: a surface this Endpoint does not serve.
        format!("/api/endpoint/{OPEN}/sta/v1.1/Things"),
        format!("/api/endpoint/{PLAIN}/ngsi-ld/v1/entities?type=AirQualityObserved"),
    ] {
        let (status, deprecation, sunset, links) = headers_of(&uri).await;
        if uri.contains(PLAIN) {
            assert!(status.is_success(), "{uri} answered {status}");
        } else {
            assert!(!status.is_success(), "{uri} answered {status}");
        }
        assert!(
            deprecation.is_empty() && sunset.is_empty() && links.is_empty(),
            "{uri}: {deprecation:?} {sunset:?} {links:?}"
        );
    }
}
