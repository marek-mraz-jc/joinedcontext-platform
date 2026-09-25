//! The endpoint as a GIS client sees it (T-0159, EP-29, EP-30, EP-31, EP-32, EP-39).
//!
//! A GIS client is not a browser: it reads the landing page once, follows the `data` link,
//! remembers the collection ids and then talks only in `bbox` and `datetime` forever. So what
//! these tests hold to are the joins between documents, not their prose: a link that goes
//! nowhere, a collection id that is not the one `items` answers on, or a conformance class
//! claimed but not implemented all fail here rather than inside QGIS.
//!
//! Two properties are the reason the representation is safe at all. Every answer comes from the
//! same projected entity page the NGSI-LD surface serves, so a policy cannot be softer here.
//! And the surface has no write half: every non-safe method is refused before a path is even
//! parsed.

mod common;

use axum::body::Body;
use axum::http::{Method, Request as HttpRequest, StatusCode};
use common::BrokerStub;
use context_gateway::app::{router, Gateway};
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::{Endpoint, Space};
use jc_core::kinds::{Audience, Representation};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use tower::ServiceExt;

const SLUG: &str = "mluyob4nz52lok3ssk7pgn5vwt";
const HOST: &str = "https://city.example";

fn endpoint(hidden: &[&str]) -> Endpoint {
    Endpoint {
        roles: Default::default(),
        slug: SLUG.to_owned(),
        title: std::collections::BTreeMap::new(),
        description: std::collections::BTreeMap::new(),
        space: "ovzdusie".to_owned(),
        project: "ovzdusie".to_owned(),
        audience: Audience::Public,
        allowed_projects: Vec::new(),
        representations: vec![Representation::NgsiLd, Representation::OgcFeatures],
        rate_limit: None,
        file_limits: None,
        hidden_attributes: hidden.iter().map(|name| (*name).to_owned()).collect(),
        projection: None,
        base_path: format!("/api/endpoint/{SLUG}"),
        models: Vec::new(),
        view_mapping: None,
        catalog: None,
        policies: vec![serde_norway::from_str(
            r#"contextSpaceRef: ovzdusie
assigner: did:web:banskabystrica.sk
assignee: { kind: role, id: public }
operations: [queryEntity, retrieveEntity]
"#,
        )
        .expect("the policy spec parses")],
    }
}

fn space(endpoint: &Endpoint) -> Space {
    Space {
        endpoint: Arc::new(endpoint.clone()),
        title: BTreeMap::from([
            ("sk".to_owned(), "Ovzdušie".to_owned()),
            ("en".to_owned(), "Air quality".to_owned()),
        ]),
        description: BTreeMap::from([("en".to_owned(), "Municipal air quality".to_owned())]),
        is_sandbox: false,
        default_locale: Some("sk".to_owned()),
    }
}

fn gateway(broker: &str, endpoint: Endpoint) -> axum::Router {
    let space = space(&endpoint);
    let mut gateway = Gateway::new(
        Broker::new(broker),
        Box::new(PolicyPdp),
        "banskabystrica.sk",
    );
    gateway.public_url = Some(HOST.to_owned());
    router(Arc::new(gateway.serve([endpoint]).serve_spaces([space])))
}

fn station(local: &str, pm10: f64, observed: &str) -> Value {
    json!({
        "id": format!("urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:ovzdusie:{local}"),
        "type": "AirQualityObserved",
        "pm10": { "type": "Property", "value": pm10, "observedAt": observed },
        "operatorPhone": { "type": "Property", "value": "+421 900 000 000" },
        "location": {
            "type": "GeoProperty",
            "value": { "type": "Point", "coordinates": [19.146, 48.736] }
        }
    })
}

/// A type the space holds that carries no geometry at all, which is not a Feature Collection.
fn advisory() -> Value {
    json!({
        "id": "urn:ngsi-ld:Advisory:banskabystrica.sk:ovzdusie:a-1",
        "type": "Advisory",
        "text": { "type": "Property", "value": "stay indoors" }
    })
}

async fn call(
    app: axum::Router,
    method: Method,
    path: &str,
) -> (StatusCode, Vec<u8>, axum::http::HeaderMap) {
    let response = app
        .oneshot(
            HttpRequest::builder()
                .method(method)
                .uri(path)
                .body(Body::empty())
                .expect("a request"),
        )
        .await
        .expect("the gateway answers");
    let status = response.status();
    let headers = response.headers().clone();
    let body = axum::body::to_bytes(response.into_body(), 8 * 1024 * 1024)
        .await
        .expect("a readable body");
    (status, body.to_vec(), headers)
}

async fn get(app: axum::Router, path: &str) -> (StatusCode, Value) {
    let (status, body, _) = call(app, Method::GET, path).await;
    let parsed = serde_json::from_slice(&body).unwrap_or(Value::Null);
    (status, parsed)
}

fn ogc(path: &str) -> String {
    format!("/api/endpoint/{SLUG}/ogc/features{path}")
}

fn hrefs(document: &Value, rel: &str) -> Vec<String> {
    document["links"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|link| link["rel"] == json!(rel))
        .filter_map(|link| link["href"].as_str().map(str::to_owned))
        .collect()
}

/// EP-29: the landing page is the only URL a client is given, so every door out of the
/// service has to be on it and has to be absolute.
#[tokio::test]
async fn the_landing_page_links_to_conformance_and_collections() {
    let broker = BrokerStub::start(vec![json!([])]).await;
    let app = gateway(&broker.url, endpoint(&[]));
    let (status, landing) = get(app, &ogc("/")).await;
    assert_eq!(status, StatusCode::OK);

    assert_eq!(
        hrefs(&landing, "conformance"),
        vec![format!(
            "{HOST}/api/endpoint/{SLUG}/ogc/features/conformance"
        )]
    );
    assert_eq!(
        hrefs(&landing, "data"),
        vec![format!(
            "{HOST}/api/endpoint/{SLUG}/ogc/features/collections"
        )]
    );
    assert_eq!(hrefs(&landing, "self").len(), 1, "exactly one self link");
    // EP-32: the title is the space's, in the locale the space declares.
    assert_eq!(landing["title"], json!("Ovzdušie"));
}

/// EP-32: a client that asks in English is answered in English, and one that asks for a
/// language nobody wrote still gets a title rather than an empty string.
#[tokio::test]
async fn the_landing_page_answers_in_the_callers_language() {
    let broker = BrokerStub::start(vec![json!([])]).await;
    for (accept, expected) in [("en-GB,en;q=0.9", "Air quality"), ("de", "Ovzdušie")] {
        let response = gateway(&broker.url, endpoint(&[]))
            .oneshot(
                HttpRequest::builder()
                    .uri(ogc("/"))
                    .header(axum::http::header::ACCEPT_LANGUAGE, accept)
                    .body(Body::empty())
                    .expect("a request"),
            )
            .await
            .expect("the gateway answers");
        let body = axum::body::to_bytes(response.into_body(), 65_536)
            .await
            .expect("a body");
        let landing: Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(landing["title"], json!(expected), "for {accept}");
    }
}

/// EP-30: a claimed class is a promise a client acts on, so the list is what the code does.
#[tokio::test]
async fn the_conformance_list_claims_only_what_is_implemented() {
    let broker = BrokerStub::start(vec![json!([])]).await;
    let (status, document) = get(gateway(&broker.url, endpoint(&[])), &ogc("/conformance")).await;
    assert_eq!(status, StatusCode::OK);

    let claimed: Vec<&str> = document["conformsTo"]
        .as_array()
        .expect("a list")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    // EP-30 names five and the endpoint claims those five; the tests below are what makes
    // each claim true, and this one only holds the list to the requirement.
    assert_eq!(
        claimed,
        vec![
            "http://www.opengis.net/spec/ogcapi-features-1/1.0/conf/core",
            "http://www.opengis.net/spec/ogcapi-features-1/1.0/conf/oas30",
            "http://www.opengis.net/spec/ogcapi-features-1/1.0/conf/geojson",
            "http://www.opengis.net/spec/ogcapi-features-2/1.0/conf/crs",
            "http://www.opengis.net/spec/ogcapi-features-3/1.0/conf/basic-cql2",
        ],
    );
}

/// EP-31: a collection is an entity type that actually carries a geometry, and its id is the
/// type's short name so a client can build the items URL from what it read.
#[tokio::test]
async fn collections_are_the_types_that_carry_a_geometry() {
    let broker = BrokerStub::start(vec![json!([
        station("st-1", 34.2, "2026-09-01T10:00:00Z"),
        advisory()
    ])])
    .await;
    let (status, document) = get(gateway(&broker.url, endpoint(&[])), &ogc("/collections")).await;
    assert_eq!(status, StatusCode::OK);

    let ids: Vec<&str> = document["collections"]
        .as_array()
        .expect("a list")
        .iter()
        .filter_map(|collection| collection["id"].as_str())
        .collect();
    assert_eq!(ids, vec!["AirQualityObserved"], "Advisory has no geometry");

    let collection = &document["collections"][0];
    assert_eq!(
        hrefs(collection, "items"),
        vec![format!(
            "{HOST}/api/endpoint/{SLUG}/ogc/features/collections/AirQualityObserved/items"
        )]
    );
    assert_eq!(
        collection["crs"],
        json!([context_gateway::translators::ogc::CRS84])
    );
}

/// EP-32: the extent is what the caller's own data spans, not a guess and not the world.
#[tokio::test]
async fn a_collection_advertises_the_extent_of_the_data_the_caller_may_see() {
    let mut west = station("st-1", 34.2, "2026-09-01T10:00:00Z");
    west["location"]["value"]["coordinates"] = json!([19.10, 48.70]);
    let mut east = station("st-2", 51.0, "2026-09-03T10:00:00Z");
    east["location"]["value"]["coordinates"] = json!([19.20, 48.76]);
    let broker = BrokerStub::start(vec![json!([west, east])]).await;

    let (status, collection) = get(
        gateway(&broker.url, endpoint(&[])),
        &ogc("/collections/AirQualityObserved"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(collection["id"], json!("AirQualityObserved"));
    assert_eq!(
        collection["extent"]["spatial"]["bbox"],
        json!([[19.10, 48.70, 19.20, 48.76]])
    );
    assert_eq!(
        collection["extent"]["temporal"]["interval"],
        json!([["2026-09-01T10:00:00Z", "2026-09-03T10:00:00Z"]])
    );
}

/// EP-31, R20: a type that is not a collection here answers the same 404 as one that does not
/// exist, so a probe over type names learns nothing.
#[tokio::test]
async fn a_type_without_a_geometry_is_not_a_collection() {
    let broker = BrokerStub::start(vec![json!([advisory()])]).await;
    let (status, _) = get(
        gateway(&broker.url, endpoint(&[])),
        &ogc("/collections/Advisory"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// EP-33, EP-37, EP-38: a feature is the entity, flattened, with the way back to what it is.
#[tokio::test]
async fn items_are_features_whose_ids_are_the_entity_urns() {
    let broker =
        BrokerStub::start(vec![json!([station("st-1", 34.2, "2026-09-01T10:00:00Z")])]).await;
    let (status, page) = get(
        gateway(&broker.url, endpoint(&[])),
        &ogc("/collections/AirQualityObserved/items"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page["type"], json!("FeatureCollection"));
    assert_eq!(page["numberReturned"], json!(1));

    let feature = &page["features"][0];
    let urn = "urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:ovzdusie:st-1";
    assert_eq!(feature["id"], json!(urn));
    assert_eq!(feature["geometry"]["type"], json!("Point"));
    assert_eq!(feature["properties"]["pm10"], json!(34.2));
    assert_eq!(
        hrefs(feature, "alternate"),
        vec![format!(
            "{HOST}/api/endpoint/{SLUG}/ngsi-ld/v1/entities/{urn}"
        )]
    );
}

/// EP-07, EP-61: a second representation is a second way to read, never a second set of rules.
#[tokio::test]
async fn a_hidden_attribute_is_absent_from_the_features() {
    let broker =
        BrokerStub::start(vec![json!([station("st-1", 34.2, "2026-09-01T10:00:00Z")])]).await;
    let (_, page) = get(
        gateway(&broker.url, endpoint(&["operatorPhone"])),
        &ogc("/collections/AirQualityObserved/items"),
    )
    .await;
    assert_eq!(
        page["features"][0]["properties"]["operatorPhone"],
        Value::Null
    );
    assert_eq!(page["features"][0]["properties"]["pm10"], json!(34.2));
}

/// EP-34: the two parameters a GIS client sends become the NGSI-LD query the broker answers.
#[tokio::test]
async fn bbox_and_datetime_reach_the_broker_as_an_ngsi_ld_query() {
    let broker =
        BrokerStub::start(vec![json!([station("st-1", 34.2, "2026-09-01T10:00:00Z")])]).await;
    let (status, _) = get(
        gateway(&broker.url, endpoint(&[])),
        &ogc("/collections/AirQualityObserved/items?bbox=19.10,48.70,19.20,48.76&datetime=2026-09-01T00:00:00Z/.."),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let hop = broker
        .hops()
        .into_iter()
        .find(|hop| hop.path == "/ngsi-ld/v1/entities")
        .expect("one entity query");
    assert!(hop.query.contains("georel=intersects"), "{}", hop.query);
    assert!(hop.query.contains("geometry=Polygon"), "{}", hop.query);
    assert!(hop.query.contains("timerel=after"), "{}", hop.query);
    assert_eq!(hop.tenant, "ovzdusie", "the tenant is pinned");
}

/// EP-34: a parameter the representation cannot honour is refused and named, because a client
/// that is told only "400" retries the same request.
#[tokio::test]
async fn an_unusable_parameter_is_refused_and_named() {
    let broker = BrokerStub::start(vec![json!([])]).await;
    for (query, parameter) in [
        ("bbox=19.10,48.70", "bbox"),
        ("bbox=19.30,48.70,19.20,48.76", "bbox"),
        ("datetime=yesterday", "datetime"),
        ("crs=http://www.opengis.net/def/crs/EPSG/0/3857", "crs"),
    ] {
        let (status, body, headers) = call(
            gateway(&broker.url, endpoint(&[])),
            Method::GET,
            &ogc(&format!("/collections/AirQualityObserved/items?{query}")),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "for {query}");
        assert_eq!(
            headers
                .get("x-parameter")
                .and_then(|value| value.to_str().ok()),
            Some(parameter),
            "for {query}"
        );
        assert!(!body.is_empty(), "a problem document for {query}");
    }
}

/// EP-36: a full page offers the next one, and following that link asks the broker for the
/// offset the cursor stands for.
#[tokio::test]
async fn a_full_page_offers_an_opaque_next_link() {
    let stations: Vec<Value> = (1..=2)
        .map(|n| {
            station(
                &format!("st-{n}"),
                30.0 + f64::from(n),
                "2026-09-01T10:00:00Z",
            )
        })
        .collect();
    let broker = BrokerStub::start(vec![json!(stations)]).await;
    let (_, page) = get(
        gateway(&broker.url, endpoint(&[])),
        &ogc("/collections/AirQualityObserved/items?limit=2"),
    )
    .await;

    let next = hrefs(&page, "next");
    let next = next.first().expect("a next link on a full page");
    assert!(
        next.contains("limit=2"),
        "the query is carried over: {next}"
    );
    let cursor = next
        .rsplit_once("next=")
        .map(|(_, cursor)| cursor.to_owned())
        .expect("a cursor");
    assert!(
        !cursor.contains('='),
        "the cursor is opaque, not an offset in the clear: {cursor}"
    );

    let follow = BrokerStub::start(vec![json!([])]).await;
    let (status, _) = get(
        gateway(&follow.url, endpoint(&[])),
        &ogc(&format!(
            "/collections/AirQualityObserved/items?limit=2&next={cursor}"
        )),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let hop = follow
        .hops()
        .into_iter()
        .find(|hop| hop.path == "/ngsi-ld/v1/entities")
        .expect("one entity query");
    assert!(hop.query.contains("offset=2"), "{}", hop.query);
}

/// A short page is the last page, so no client is sent round again for nothing.
#[tokio::test]
async fn a_short_page_offers_no_next_link() {
    let broker =
        BrokerStub::start(vec![json!([station("st-1", 34.2, "2026-09-01T10:00:00Z")])]).await;
    let (_, page) = get(
        gateway(&broker.url, endpoint(&[])),
        &ogc("/collections/AirQualityObserved/items?limit=10"),
    )
    .await;
    assert!(hrefs(&page, "next").is_empty());
}

/// EP-33, R20: an entity the caller may not read and one that does not exist answer alike.
#[tokio::test]
async fn a_feature_that_is_not_there_answers_404() {
    let broker = BrokerStub::start(vec![json!([])]).await;
    let (status, _) = get(
        gateway(&broker.url, endpoint(&[])),
        &ogc("/collections/AirQualityObserved/items/urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:ovzdusie:nope"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// EP-39: the representation has no write half, on any path, with any method.
#[tokio::test]
async fn every_non_safe_method_is_refused() {
    let broker = BrokerStub::start(vec![json!([])]).await;
    for method in [Method::POST, Method::PUT, Method::DELETE, Method::PATCH] {
        for path in [
            ogc("/"),
            ogc("/collections"),
            ogc("/collections/AirQualityObserved/items"),
        ] {
            let (status, _, headers) =
                call(gateway(&broker.url, endpoint(&[])), method.clone(), &path).await;
            assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED, "{method} {path}");
            assert_eq!(
                headers
                    .get(axum::http::header::ALLOW)
                    .and_then(|v| v.to_str().ok()),
                Some("GET, HEAD, OPTIONS"),
                "{method} {path}"
            );
        }
    }
    assert!(
        broker.hops().is_empty(),
        "a refused method reaches no broker"
    );
}

/// A client discovers the read-only fact rather than guessing it.
#[tokio::test]
async fn options_answers_with_the_safe_methods() {
    let broker = BrokerStub::start(vec![json!([])]).await;
    let (status, _, headers) = call(
        gateway(&broker.url, endpoint(&[])),
        Method::OPTIONS,
        &ogc("/collections"),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(
        headers
            .get(axum::http::header::ALLOW)
            .and_then(|v| v.to_str().ok()),
        Some("GET, HEAD, OPTIONS")
    );
}

/// EP-05: an endpoint that does not enable the representation has no OGC surface at all, and
/// says so the same way an unknown slug does.
#[tokio::test]
async fn an_endpoint_without_the_representation_answers_404() {
    let broker = BrokerStub::start(vec![json!([])]).await;
    let mut without = endpoint(&[]);
    without.representations = vec![Representation::NgsiLd];
    let (status, _) = get(gateway(&broker.url, without), &ogc("/collections")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// A path off the Core tree is not a resource, however plausible it looks.
#[tokio::test]
async fn an_unmapped_path_answers_404() {
    let broker = BrokerStub::start(vec![json!([])]).await;
    for path in ["/collections/X/items/y/z", "/collections/X/queryables"] {
        let (status, _) = get(gateway(&broker.url, endpoint(&[])), &ogc(path)).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "for {path}");
    }
}

/// Belt and braces on the fixture: the endpoint under test really does hide nothing by
/// default, so the projection tests above are measuring the policy and not the fixture.
#[test]
fn the_fixture_hides_nothing_by_default() {
    assert_eq!(endpoint(&[]).hidden_attributes, BTreeSet::new());
}

/// EP-40: the API description is generated from the endpoint, so what it lists is what this
/// caller can actually reach. Held to the shape a client parses — the version, the servers, a
/// `get` on every served path and no write half anywhere — rather than to prose.
#[tokio::test]
async fn the_api_document_describes_every_path_this_endpoint_serves() {
    let broker =
        BrokerStub::start(vec![json!([station("st-1", 34.2, "2026-09-01T10:00:00Z")])]).await;
    let (status, body, headers) = call(
        gateway(&broker.url, endpoint(&[])),
        Method::GET,
        &ogc("/api"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("application/vnd.oai.openapi+json;version=3.0"),
    );

    let document: Value = serde_json::from_slice(&body).expect("the document is JSON");
    assert_eq!(document["openapi"], json!("3.0.3"));
    assert_eq!(
        document["servers"][0]["url"],
        json!(format!("{HOST}/api/endpoint/{SLUG}/ogc/features")),
    );

    let paths = document["paths"].as_object().expect("a path map");
    for served in [
        "/",
        "/api",
        "/conformance",
        "/collections",
        "/collections/{collectionId}",
        "/collections/{collectionId}/items",
        "/collections/{collectionId}/items/{featureId}",
    ] {
        let path = paths.get(served).unwrap_or_else(|| panic!("{served}"));
        assert!(path.get("get").is_some(), "{served} has no get operation");
        assert!(
            path["get"]["responses"]["200"].is_object(),
            "{served} describes no answer"
        );
    }
    // EP-39: there is no write half, so a client reading this document finds none to try.
    for verb in ["put", "post", "patch", "delete"] {
        assert!(
            !body
                .windows(verb.len() + 3)
                .any(|w| w == format!("\"{verb}\":").as_bytes()),
            "the document offers {verb}"
        );
    }
    // The collections a caller may not see are not in the enum they would have to guess from.
    assert_eq!(
        paths["/collections/{collectionId}"]["parameters"][0]["schema"]["enum"],
        json!(["AirQualityObserved"]),
    );
}

/// EP-40: the landing page is the only URL a client is given, so the description has to be
/// reachable from it under the relation OGC defines for it.
#[tokio::test]
async fn the_landing_page_points_at_the_api_document() {
    let broker = BrokerStub::start(vec![json!([])]).await;
    let (status, document) = get(gateway(&broker.url, endpoint(&[])), &ogc("/")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        hrefs(&document, "service-desc"),
        vec![format!("{HOST}/api/endpoint/{SLUG}/ogc/features/api")],
    );
}

/// EP-35: every operator of the subset reaches the broker as the NGSI-LD parameter it means.
///
/// The whole point of the class: a filter box in QGIS produces these, and each one has to
/// narrow the upstream query rather than be dropped on the way. Asserted on the hop, because
/// what matters is what the broker was asked, not what the gateway answered.
#[tokio::test]
async fn every_supported_cql2_operator_reaches_the_broker() {
    // The caller's filter arrives as one parenthesized term, which is how the PDP conjoins it
    // with the grants' own: no expression can pair a caller's operator with a grant's operand.
    for (filter, expected) in [
        ("pm10 > 50", "q=(pm10>50)"),
        // The spacing a client actually sends, which the scanner separates on its own.
        ("pm10>50", "q=(pm10>50)"),
        ("pm10 = 50", "q=(pm10==50)"),
        ("pm10 <> 50", "q=(pm10!=50)"),
        ("pm10 BETWEEN 10 AND 20", "q=(pm10==10..20)"),
        ("pm10 IN (10,20)", "q=(pm10==10,20)"),
        ("pm10 IS NULL", "q=(!pm10)"),
        ("pm10 IS NOT NULL", "q=(pm10)"),
        ("name LIKE 'Kal%'", "q=(name~=\"^Kal.*$\")"),
        ("pm10 > 50 AND pm25 < 20", "q=(pm10>50;pm25<20)"),
        ("pm10 > 50 OR pm25 < 20", "q=((pm10>50|pm25<20))"),
        ("NOT pm10 > 50", "q=(pm10<=50)"),
        (
            "T_AFTER(observedAt, TIMESTAMP('2026-09-01T00:00:00Z'))",
            "timerel=after",
        ),
        (
            "T_BEFORE(observedAt, TIMESTAMP('2026-09-01T00:00:00Z'))",
            "timerel=before",
        ),
        (
            "T_DURING(observedAt, INTERVAL('2026-09-01T00:00:00Z','2026-09-02T00:00:00Z'))",
            "timerel=between",
        ),
        (
            "S_INTERSECTS(location, POLYGON((19.1 48.7, 19.2 48.7, 19.2 48.8, 19.1 48.7)))",
            "georel=intersects",
        ),
        (
            "S_WITHIN(location, BBOX(19.1,48.7,19.2,48.8))",
            "georel=within",
        ),
        (
            "NOT S_INTERSECTS(location, POINT(19.1 48.7))",
            "georel=disjoint",
        ),
    ] {
        let broker = BrokerStub::start(vec![json!([])]).await;
        let (status, _) = get(
            gateway(&broker.url, endpoint(&[])),
            &ogc(&format!(
                "/collections/AirQualityObserved/items?filter={}&filter-lang=cql2-text",
                urlencode(filter),
            )),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "for {filter}");
        let hop = broker
            .hops()
            .into_iter()
            .find(|hop| hop.path == "/ngsi-ld/v1/entities")
            .unwrap_or_else(|| panic!("no entity query for {filter}"));
        let sent = percent_decode(&hop.query);
        assert!(
            sent.contains(expected),
            "{filter} sent {sent}, expected {expected}"
        );
    }
}

/// EP-35: what the subset does not cover is refused with the operator named, never applied
/// half way. A filter that is silently dropped returns rows the caller asked not to see.
#[tokio::test]
async fn an_unsupported_cql2_construct_is_refused_with_its_name() {
    for (filter, named) in [
        ("ACCENTI(name) = 'Kallio'", "ACCENTI"),
        ("CASEI(name) = 'kallio'", "CASEI"),
        ("NOT name LIKE 'Kal%'", "NOT LIKE"),
        (
            "NOT S_WITHIN(location, BBOX(19.1,48.7,19.2,48.8))",
            "NOT S_WITHIN",
        ),
        (
            "pm10 > 50 OR S_WITHIN(location, BBOX(19.1,48.7,19.2,48.8))",
            "whole query",
        ),
        ("pm10 >", "expected a value"),
        ("   ", "empty"),
    ] {
        let broker = BrokerStub::start(vec![json!([])]).await;
        let (status, body, headers) = call(
            gateway(&broker.url, endpoint(&[])),
            Method::GET,
            &ogc(&format!(
                "/collections/AirQualityObserved/items?filter={}&filter-lang=cql2-text",
                urlencode(filter),
            )),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "for {filter:?}");
        assert_eq!(
            headers
                .get("x-parameter")
                .and_then(|value| value.to_str().ok()),
            Some("filter"),
            "for {filter:?}"
        );
        let detail = String::from_utf8_lossy(&body).to_string();
        assert!(
            detail.contains(named),
            "{filter:?} was refused as {detail}, which does not name {named}"
        );
        assert!(
            broker
                .hops()
                .iter()
                .all(|hop| hop.path != "/ngsi-ld/v1/entities"),
            "{filter:?} reached the broker anyway"
        );
    }
}

/// EP-35: a filter language this endpoint does not read is refused rather than guessed at.
#[tokio::test]
async fn a_filter_in_another_language_is_refused() {
    let broker = BrokerStub::start(vec![json!([])]).await;
    let (status, _, headers) = call(
        gateway(&broker.url, endpoint(&[])),
        Method::GET,
        &ogc("/collections/AirQualityObserved/items?filter=pm10%3E50&filter-lang=cql2-json"),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        headers
            .get("x-parameter")
            .and_then(|value| value.to_str().ok()),
        Some("filter-lang"),
    );
}

/// NGSI-LD carries one `geoQ` and one `temporalQ`, so `bbox` and a spatial filter cannot both
/// be applied. Refused rather than half applied: dropping one of them widens the answer.
#[tokio::test]
async fn a_filter_and_the_parameter_that_means_the_same_thing_cannot_both_be_sent() {
    for query in [
        "bbox=19.1,48.7,19.2,48.8&filter=S_WITHIN(location,BBOX(19.1,48.7,19.2,48.8))",
        "datetime=2026-09-01T00:00:00Z/..&filter=T_AFTER(observedAt,TIMESTAMP('2026-09-01T00:00:00Z'))",
    ] {
        let broker = BrokerStub::start(vec![json!([])]).await;
        let (status, _, headers) = call(
            gateway(&broker.url, endpoint(&[])),
            Method::GET,
            &ogc(&format!("/collections/AirQualityObserved/items?{}", urlencode_query(query))),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "for {query}");
        assert_eq!(
            headers
                .get("x-parameter")
                .and_then(|value| value.to_str().ok()),
            Some("filter"),
            "for {query}"
        );
    }
}

/// GW10: a filter is caller input compiled into an upstream query, so the grants still apply
/// to it. The endpoint's own policy carries a `q`, and both survive into the one sent.
#[tokio::test]
async fn a_filter_is_intersected_with_the_grants_and_never_replaces_them() {
    let broker = BrokerStub::start(vec![json!([])]).await;
    let mut restricted = endpoint(&[]);
    restricted.policies = vec![serde_norway::from_str(
        r#"contextSpaceRef: ovzdusie
assigner: did:web:banskabystrica.sk
assignee: { kind: role, id: public }
operations: [queryEntity, retrieveEntity]
q: "pm10>=0"
"#,
    )
    .expect("the policy spec parses")];

    let (status, _) = get(
        gateway(&broker.url, restricted),
        &ogc("/collections/AirQualityObserved/items?filter=pm10%20%3E%2050"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let hop = broker
        .hops()
        .into_iter()
        .find(|hop| hop.path == "/ngsi-ld/v1/entities")
        .expect("one entity query");
    let sent = percent_decode(&hop.query);
    assert!(
        sent.contains("pm10>50"),
        "the caller's filter is gone: {sent}"
    );
    assert!(
        sent.contains("pm10>=0"),
        "the grant's own filter is gone: {sent}"
    );
}

/// Minimal percent-encoding for the query strings these tests build.
fn urlencode(raw: &str) -> String {
    raw.bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (byte as char).to_string()
            }
            other => format!("%{other:02X}"),
        })
        .collect()
}

/// The same, but leaving the `&` and `=` that separate the parameters alone.
fn urlencode_query(raw: &str) -> String {
    raw.split('&')
        .map(|pair| match pair.split_once('=') {
            Some((name, value)) => format!("{name}={}", urlencode(value)),
            None => urlencode(pair),
        })
        .collect::<Vec<_>>()
        .join("&")
}

/// Enough percent-decoding to read back a query the gateway built.
fn percent_decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] == b'%' && at + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(&raw[at + 1..at + 3], 16) {
                out.push(byte);
                at += 3;
                continue;
            }
        }
        out.push(bytes[at]);
        at += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}
