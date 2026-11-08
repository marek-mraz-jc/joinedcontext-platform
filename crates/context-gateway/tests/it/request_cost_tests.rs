//! The broker's request cost, per Organization (T-6616, API/00 §6a).
//!
//! A broker with `ANTARES_REQUEST_COST=on` answers `Server-Timing` and `JC-Request-Cost`. The
//! gateway takes both off the answer unless `JC_GATEWAY_REQUEST_COST=header` and the caller is
//! a verified member of the Endpoint's own Organization; under `metrics` and `header` it adds the
//! figures to counters labelled by the caller's Organization, read from the verified token only.

use crate::common;

use axum::body::Body;
use axum::http::{header, HeaderMap, Request, StatusCode};
use context_gateway::app::{router, Gateway};
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::request_cost::Mode;
use context_gateway::resolver::Endpoint;
use context_gateway::store::Provenance;
use jc_core::kinds::{Audience, PolicySpec, Representation};
use serde_json::json;
use std::sync::Arc;
use tower::ServiceExt;

/// The Endpoint's project, of Organization `hel`.
const PROJECT: &str = "helsinki-mobility";
/// A project of Organization `zilina`.
const OUTSIDER: &str = "zilina";
const TIMING: &str = "app;dur=2.5, db;dur=1.3, pool;dur=0.4, total;dur=3.2";
const COST: &str = "cpu-ms=2.463, db-ms=1.291, statements=3, rows=5";

fn endpoint(slug: &str) -> Endpoint {
    let policy: PolicySpec = serde_norway::from_str(
        "contextSpaceRef: bikes\nassigner: did:web:hel.fi\n\
         assignee: { kind: role, id: public }\noperations: [queryEntity]\n\
         information:\n  - entities:\n      - type: BikeHireDockingStation\n",
    )
    .expect("the policy spec parses");
    Endpoint {
        declared_types: None,
        roles: Default::default(),
        slug: slug.to_owned(),
        title: Default::default(),
        description: Default::default(),
        space: "bikes".to_owned(),
        project: PROJECT.to_owned(),
        audience: Audience::Public,
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
        page: None,
        policy_names: Vec::new(),
        manifest_name: String::new(),
        policies: vec![policy],
    }
}

/// A broker that measures every request, or none when `measured` is false.
async fn broker(measured: bool) -> String {
    let app = axum::Router::new().fallback(move || async move {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONTENT_TYPE,
            "application/json".parse().expect("a value"),
        );
        if measured {
            headers.insert("server-timing", TIMING.parse().expect("a value"));
            headers.insert("jc-request-cost", COST.parse().expect("a value"));
        }
        (headers, "[]")
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a free port");
    let address = listener.local_addr().expect("the bound address");
    tokio::spawn(async move {
        let _ = axum::serve(listener, common::plain_tree(app)).await;
    });
    format!("http://{address}")
}

struct Fixture {
    realm: common::Realm,
    app: axum::Router,
    slug: &'static str,
}

async fn fixture(slug: &'static str, mode: Mode, measured: bool) -> Fixture {
    let realm = common::Realm::new();
    let mut gateway = Gateway::new(
        Broker::new(broker(measured).await),
        Box::new(PolicyPdp),
        "hel.fi",
    )
    .serve([endpoint(slug)])
    .authenticate(Arc::new(realm.verifier()), Default::default(), None);
    gateway.replace_provenance(Provenance {
        organizations: [(PROJECT, "hel"), (OUTSIDER, "zilina")]
            .into_iter()
            .map(|(project, organization)| (project.to_owned(), organization.to_owned()))
            .collect(),
        ..Default::default()
    });
    gateway.request_cost = mode;
    Fixture {
        realm,
        app: router(Arc::new(gateway)),
        slug,
    }
}

fn person(fixture: &Fixture, groups: &[&str]) -> String {
    fixture.realm.mint(&json!({
        "iss": common::ISSUER,
        "sub": "f:1:someone",
        "aud": fixture.slug,
        "exp": common::in_seconds(300),
        "iat": common::in_seconds(-10),
        "preferred_username": "demo.someone",
        "groups": groups,
        "realm_access": { "roles": ["member"] },
    }))
}

/// A read with `token` (anonymous when `None`) and any `extra` request headers: the status and
/// the answer's headers.
async fn read(
    fixture: &Fixture,
    token: Option<&str>,
    extra: &[(&str, &str)],
) -> (StatusCode, HeaderMap) {
    let mut request = Request::builder().uri(format!(
        "/api/endpoint/{}/ngsi-ld/v1/entities?type=BikeHireDockingStation",
        fixture.slug
    ));
    if let Some(token) = token {
        request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    for (name, value) in extra {
        request = request.header(*name, *value);
    }
    let response = fixture
        .app
        .clone()
        .oneshot(request.body(Body::empty()).expect("a request"))
        .await
        .expect("the gateway answers");
    (response.status(), response.headers().clone())
}

/// `/metrics` as text.
async fn scrape(fixture: &Fixture) -> String {
    let response = fixture
        .app
        .clone()
        .oneshot(
            Request::get("/metrics")
                .body(Body::empty())
                .expect("a request"),
        )
        .await
        .expect("the gateway answers");
    let body = axum::body::to_bytes(response.into_body(), 8 * 1024 * 1024)
        .await
        .expect("a readable body");
    String::from_utf8(body.to_vec()).expect("text")
}

/// The value of `series` for `organization` on the fixture's Endpoint, 0 when there is none.
fn value(text: &str, series: &str, organization: &str, slug: &str) -> u64 {
    text.lines()
        .filter(|line| line.starts_with(&format!("{series}{{")))
        .filter(|line| {
            line.contains(&format!("organization=\"{organization}\""))
                && line.contains(&format!("endpoint=\"{slug}\""))
        })
        .filter_map(|line| line.rsplit(' ').next()?.parse::<u64>().ok())
        .sum()
}

fn timings(headers: &HeaderMap) -> Vec<String> {
    headers
        .get_all("server-timing")
        .iter()
        .filter_map(|value| value.to_str().ok().map(str::to_owned))
        .collect()
}

/// Off, the default: neither broker header reaches the caller and no counter exists for the
/// Endpoint; the gateway's own Server-Timing stays as it was (T-5042).
#[tokio::test]
async fn off_hands_on_nothing_and_counts_nothing() {
    let fixture = fixture("c0stoffq2mzt6vhx3nbwrs5cjd", Mode::Off, true).await;
    let token = person(&fixture, &[PROJECT]);
    let (status, headers) = read(&fixture, Some(&token), &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert!(headers.get("jc-request-cost").is_none(), "{headers:?}");
    assert_eq!(timings(&headers).len(), 1, "{headers:?}");
    assert!(timings(&headers)[0].starts_with("pdp;dur="), "{headers:?}");
    let text = scrape(&fixture).await;
    assert!(
        !text
            .lines()
            .any(|line| line.starts_with("jc_gateway_request_cost") && line.contains(fixture.slug)),
        "no series for the Endpoint"
    );
}

/// Metrics: two reads by members of two Organizations move only their own Organization's
/// counters, and neither header reaches either caller.
#[tokio::test]
async fn metrics_counts_each_read_under_the_callers_organization() {
    let fixture = fixture("c0stmetricsq2mzt6vhx3nbwrs", Mode::Metrics, true).await;
    let hel = person(&fixture, &[PROJECT]);
    let zilina = person(&fixture, &[OUTSIDER]);
    let (status, headers) = read(&fixture, Some(&hel), &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert!(headers.get("jc-request-cost").is_none(), "{headers:?}");
    let text = scrape(&fixture).await;
    let rows = |text: &str, organization| {
        value(
            text,
            "jc_gateway_request_cost_rows_total",
            organization,
            fixture.slug,
        )
    };
    assert_eq!((rows(&text, "hel"), rows(&text, "zilina")), (5, 0));

    let (status, headers) = read(&fixture, Some(&zilina), &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert!(headers.get("jc-request-cost").is_none(), "{headers:?}");
    let text = scrape(&fixture).await;
    assert_eq!((rows(&text, "hel"), rows(&text, "zilina")), (5, 5));
    for (series, expected) in [
        ("jc_gateway_request_cost_requests_total", 1),
        ("jc_gateway_request_cost_statements_total", 3),
        ("jc_gateway_request_cost_app_microseconds_total", 2500),
        ("jc_gateway_request_cost_db_microseconds_total", 1300),
        ("jc_gateway_request_cost_pool_microseconds_total", 400),
    ] {
        assert_eq!(
            value(&text, series, "hel", fixture.slug),
            expected,
            "{series}"
        );
        assert_eq!(
            value(&text, series, "zilina", fixture.slug),
            expected,
            "{series}"
        );
    }
    println!(
        "{}",
        text.lines()
            .filter(|line| line.contains(fixture.slug))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// The label comes from the verified token alone: a forged `jc_org` or `x-jc-*` request header
/// moves nothing, and an anonymous read counts under `public`.
#[tokio::test]
async fn a_forged_organization_header_changes_nothing() {
    let fixture = fixture("c0stforgedq2mzt6vhx3nbwrs5", Mode::Metrics, true).await;
    let hel = person(&fixture, &[PROJECT]);
    let forged = [
        ("jc_org", "zilina"),
        ("x-jc-organization", "zilina"),
        ("x-jc-client", "zilina"),
    ];
    read(&fixture, Some(&hel), &forged).await;
    read(&fixture, None, &forged).await;
    let text = scrape(&fixture).await;
    let rows = |organization| {
        value(
            &text,
            "jc_gateway_request_cost_rows_total",
            organization,
            fixture.slug,
        )
    };
    assert_eq!((rows("hel"), rows("public"), rows("zilina")), (5, 5, 0));
}

/// An answer the broker did not measure adds nothing, in any mode.
#[tokio::test]
async fn an_unmeasured_answer_adds_nothing() {
    let fixture = fixture("c0stunmeasuredq2mzt6vhx3nb", Mode::Header, false).await;
    let token = person(&fixture, &[PROJECT]);
    let (status, headers) = read(&fixture, Some(&token), &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert!(headers.get("jc-request-cost").is_none());
    assert!(
        !timings(&headers).concat().contains("gw;dur="),
        "{headers:?}"
    );
    let text = scrape(&fixture).await;
    assert!(!text
        .lines()
        .any(|line| line.contains(fixture.slug) && line.starts_with("jc_gateway_request_cost")));
}

/// Header: the Endpoint's own Organization gets both headers and the gateway's `gw` share;
/// another Organization and an anonymous caller get neither, and are still counted.
#[tokio::test]
async fn header_hands_the_cost_to_the_endpoints_own_organization_only() {
    let fixture = fixture("c0stheaderq2mzt6vhx3nbwrs5", Mode::Header, true).await;
    let hel = person(&fixture, &[PROJECT]);
    let (status, headers) = read(&fixture, Some(&hel), &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers.get("jc-request-cost").expect("the cost"), COST);
    let timing = timings(&headers);
    assert_eq!(
        timing.first().map(String::as_str),
        Some(TIMING),
        "{timing:?}"
    );
    assert!(
        timing.last().expect("the gateway's").contains(", gw;dur="),
        "{timing:?}"
    );

    let zilina = person(&fixture, &[OUTSIDER]);
    for token in [Some(zilina.as_str()), None] {
        let (status, headers) = read(&fixture, token, &[]).await;
        assert_eq!(status, StatusCode::OK);
        assert!(headers.get("jc-request-cost").is_none(), "{headers:?}");
        assert_eq!(timings(&headers).len(), 1, "{headers:?}");
        assert!(!timings(&headers)[0].contains("gw;dur="), "{headers:?}");
    }
    let text = scrape(&fixture).await;
    let requests = |organization| {
        value(
            &text,
            "jc_gateway_request_cost_requests_total",
            organization,
            fixture.slug,
        )
    };
    assert_eq!(
        (requests("hel"), requests("zilina"), requests("public")),
        (1, 1, 1)
    );
}
