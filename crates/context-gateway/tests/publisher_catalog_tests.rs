//! `GET /catalog/{project}.jsonld` and `.ttl`: one publisher's catalogue (T-4586, EP-127, API/08
//! section 6).
//!
//! Contract, in one sentence: a Project that names its publisher in `spec.catalog` has a
//! `dcat:Catalog` of its own, holding the record of each of its public Endpoints as the anonymous
//! caller reads it, under that publisher, and nothing of any other Endpoint; every other name is
//! the same 404. The input that could widen it is a token: the route reads none.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use context_gateway::app::{router, Gateway};
use context_gateway::auth::accounts::ServiceAccounts;
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::Endpoint;
use context_gateway::store::{self, ProjectPublisher, Publishers};
use jc_core::kinds::{
    Audience, ContactPoint, DcatProfile, PolicySpec, ProjectCatalog, Publisher, Representation,
};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tower::ServiceExt;

const CITY: &str = "k4y7pq2mzt6vhx3nbwrs5cjd8f";
const CITY_TOO: &str = "m3x8rq5vzt2khw7pbnd4sjc6fy";
const CITY_CLOSED: &str = "p9d2wc5kzn8mth4rqvb7xj3sfy";
const REGION: &str = "w2b6tq9mzx4kcr7nvh3dsj5pfy";
const PRAGUE: &str = "t6v3kq8mzx2wcr5nbh7dsj4pfy";
/// The public door the Portal generated for the city's App `mapa` (T-4596).
const APP_DOOR: &str = "a7n4kq2mzx6wcr3vbh5dsj8pfy";
const HOST: &str = "https://bb.example.sk";
const IRI: &str = "https://data.gov.sk/id/legal-subject/00313271";

fn endpoint(slug: &str, project: &str, space: &str, audience: Audience) -> Endpoint {
    let policy: PolicySpec = serde_norway::from_str(&format!(
        "contextSpaceRef: {space}\n\
         assigner: did:web:banskabystrica.sk\n\
         assignee: {{ kind: role, id: public }}\n\
         operations: [queryEntity, retrieveEntity]\n\
         information:\n  - entities:\n      - type: AirQualityObserved\n"
    ))
    .expect("the policy spec parses");
    Endpoint {
        declared_types: None,
        roles: Default::default(),
        slug: slug.to_owned(),
        title: [("sk".to_owned(), format!("Dáta {space}"))].into(),
        description: [
            ("cs".to_owned(), format!("Měření {space}")),
            ("sk".to_owned(), format!("Merania {space}")),
        ]
        .into(),
        space: space.to_owned(),
        project: project.to_owned(),
        audience,
        allowed_projects: Vec::new(),
        representations: vec![Representation::NgsiLd, Representation::Csv],
        rate_limit: None,
        creates: None,
        file_limits: None,
        hidden_attributes: Default::default(),
        projection: None,
        base_path: format!("/api/endpoint/{slug}"),
        models: Vec::new(),
        view_mapping: None,
        catalog: Some(Arc::new(complete(project))),
        policy_names: Vec::new(),
        manifest_name: String::new(),
        policies: vec![policy],
    }
}

/// Every member the national profiles ask of a record, with the publisher the store folds in
/// from the project (`store::load`), so the written catalogues hold the gateway's whole record.
fn complete(project: &str) -> jc_core::kinds::Catalog {
    // The HVD regulation belongs only to a dataset with an HVD category (the Slovak shapes'
    // `sh:xone`); the Czech record names the open-data act it is published under.
    let (publisher, spatial, legislation) = if project == "praha" {
        let act = "https://www.e-sbirka.cz/eli/cz/sb/1999/106/".to_owned();
        (prague().catalog.publisher, "CZ010", vec![act])
    } else {
        (publisher().catalog.publisher, "SK032", Vec::new())
    };
    jc_core::kinds::Catalog {
        publisher: Some(publisher),
        license: Some(jc_core::kinds::Licence::CcBy4),
        themes: vec![jc_core::kinds::DataTheme::Envi],
        keywords: [
            ("cs".to_owned(), vec!["ovzduší".to_owned()]),
            ("sk".to_owned(), vec!["ovzdušie".to_owned()]),
        ]
        .into(),
        spatial: vec![spatial.to_owned()],
        frequency: Some(jc_core::kinds::Frequency::Hourly),
        applicable_legislation: legislation,
        personal_data: Some(jc_core::kinds::PersonalData::DoesNotContain),
        ..Default::default()
    }
}

/// `app-mapa`, the Endpoint the Portal writes for App `mapa`: public as the App is, with no
/// catalogue block of its own.
fn app_door() -> Endpoint {
    let mut door = endpoint(APP_DOOR, "banskabystrica", "verejne", Audience::Public);
    door.roles.app_client = Some("app-mapa".to_owned());
    door.catalog = None;
    door
}

fn publisher() -> ProjectPublisher {
    ProjectPublisher {
        title: [
            ("en".to_owned(), "City of Banská Bystrica".to_owned()),
            ("sk".to_owned(), "Mesto Banská Bystrica".to_owned()),
        ]
        .into(),
        description: Default::default(),
        domain: Some("banskabystrica.sk".to_owned()),
        catalog: ProjectCatalog {
            publisher: Publisher {
                name: [("sk".to_owned(), "Mesto Banská Bystrica".to_owned())].into(),
                uri: Some(IRI.to_owned()),
            },
            contact_point: Some(ContactPoint {
                name: "Oddelenie informatiky".to_owned(),
                email: "opendata@example.org".to_owned(),
            }),
        },
    }
}

/// A Czech publisher, named by its RPP IRI the way DCAT-AP-CZ asks.
fn prague() -> ProjectPublisher {
    ProjectPublisher {
        title: [
            ("cs".to_owned(), "Hlavní město Praha".to_owned()),
            ("en".to_owned(), "City of Prague".to_owned()),
        ]
        .into(),
        description: Default::default(),
        domain: Some("praha.eu".to_owned()),
        catalog: ProjectCatalog {
            publisher: Publisher {
                name: [("cs".to_owned(), "Hlavní město Praha".to_owned())].into(),
                uri: Some(
                    "https://rpp-opendata.egon.gov.cz/odrpp/zdroj/orgán-veřejné-moci/00064581"
                        .to_owned(),
                ),
            },
            contact_point: Some(ContactPoint {
                name: "Odbor informatiky".to_owned(),
                email: "opendata@example.org".to_owned(),
            }),
        },
    }
}

/// The city publishes two public Endpoints, one for its staff and its App's public door; the
/// region publishes one and names no publisher; `quiet` names a publisher and has no public
/// Endpoint.
fn app_of(realm: &common::Realm, listed: &[DcatProfile]) -> axum::Router {
    let gateway = Gateway::new(
        Broker::new("http://127.0.0.1:1"),
        Box::new(PolicyPdp),
        "banskabystrica.sk",
    )
    .serve(vec![
        endpoint(CITY, "banskabystrica", "ovzdusie", Audience::Public),
        endpoint(CITY_TOO, "banskabystrica", "doprava", Audience::Public),
        endpoint(
            CITY_CLOSED,
            "banskabystrica",
            "interne",
            Audience::Organization,
        ),
        app_door(),
        endpoint(REGION, "bbsk", "cesty", Audience::Public),
        endpoint(PRAGUE, "praha", "ovzdusi", Audience::Public),
        endpoint(
            "q8n3vz5tk2wmx7rbc4hdj6sfpy",
            "quiet",
            "tiche",
            Audience::Organization,
        ),
    ])
    .authenticate(
        Arc::new(realm.verifier()),
        ServiceAccounts::new(),
        Some(HOST.to_owned()),
    );
    gateway.replace_limits(store::Limits {
        dcat_profiles: jc_dcat::profile::Offered::of(listed),
        ..Default::default()
    });
    let publishers: Publishers = [
        ("banskabystrica".to_owned(), Arc::new(publisher())),
        ("quiet".to_owned(), Arc::new(publisher())),
        ("praha".to_owned(), Arc::new(prague())),
    ]
    .into();
    gateway.replace_publishers(publishers);
    router(Arc::new(gateway))
}

async fn get(app: axum::Router, path: &str, token: Option<&str>) -> (StatusCode, String, String) {
    let mut builder = Request::builder().uri(path);
    if let Some(token) = token {
        builder = builder.header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let response = app
        .oneshot(builder.body(Body::empty()).expect("a request"))
        .await
        .expect("the gateway answers");
    let status = response.status();
    let media = response
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let body = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024)
        .await
        .expect("a readable body");
    (status, media, String::from_utf8_lossy(&body).into_owned())
}

fn datasets(body: &str) -> Vec<String> {
    let feed: Value = serde_json::from_str(body).expect("JSON-LD");
    let mut ids: Vec<String> = feed["dcat:dataset"]
        .as_array()
        .expect("datasets")
        .iter()
        .filter_map(|d| d["@id"].as_str().map(str::to_owned))
        .collect();
    ids.sort_unstable();
    ids
}

#[tokio::test]
async fn a_publishers_catalogue_lists_its_public_endpoints_under_its_publisher_only() {
    let realm = common::Realm::new();
    let (status, media, body) =
        get(app_of(&realm, &[]), "/catalog/banskabystrica.jsonld", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(media, "application/ld+json");
    let feed: Value = serde_json::from_str(&body).expect("JSON-LD");
    assert_eq!(feed["@type"], "dcat:Catalog");
    assert_eq!(feed["@id"], format!("{HOST}/catalog/banskabystrica"));
    assert_eq!(feed["dct:publisher"]["@id"], IRI);
    assert_eq!(feed["foaf:homepage"], "https://banskabystrica.sk/");
    assert_eq!(
        feed["dcat:contactPoint"]["vcard:hasEmail"],
        "mailto:opendata@example.org"
    );
    assert_eq!(
        datasets(&body),
        vec![
            format!("{HOST}/api/endpoint/{CITY}"),
            format!("{HOST}/api/endpoint/{CITY_TOO}"),
        ]
    );
    for other in [CITY_CLOSED, "interne", REGION, "cesty", APP_DOOR] {
        assert!(!body.contains(other), "the catalogue names {other}: {body}");
    }
}

/// T-4596: an App's door re-serves a space for that App and has no catalogue block, so it is
/// no dataset of the publisher, in its catalogue or in the installation's; its record stays.
#[tokio::test]
async fn an_apps_door_is_in_no_catalogue_and_keeps_its_record() {
    let realm = common::Realm::new();
    for path in ["/catalog/banskabystrica.jsonld", "/catalog.jsonld"] {
        let (status, _, body) = get(app_of(&realm, &[]), path, None).await;
        assert_eq!(status, StatusCode::OK, "{path}: {body}");
        assert!(
            datasets(&body).contains(&format!("{HOST}/api/endpoint/{CITY}")),
            "{body}"
        );
        assert!(
            !body.contains(APP_DOOR),
            "{path} lists the App's door: {body}"
        );
    }
    let (status, _, _) = get(
        app_of(&realm, &[]),
        &format!("/api/endpoint/{APP_DOOR}/"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn the_turtle_says_the_same_and_a_members_token_widens_nothing() {
    let realm = common::Realm::new();
    let member = realm.mint(&json!({
        "iss": common::ISSUER,
        "sub": "an-employee",
        "aud": CITY_CLOSED,
        "preferred_username": "jana",
        "groups": ["/interne"],
        "exp": common::in_seconds(300),
        "iat": common::in_seconds(-10),
    }));
    let path = "/catalog/banskabystrica.ttl";
    let (_, _, anonymous) = get(app_of(&realm, &[]), path, None).await;
    let (status, media, body) = get(app_of(&realm, &[]), path, Some(&member)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(media, "text/turtle");
    assert_eq!(
        body, anonymous,
        "a token changes nothing the catalogue says"
    );
    assert!(body.contains(&format!("dct:publisher <{IRI}>")), "{body}");
    assert!(body.contains(&format!("<{HOST}/api/endpoint/{CITY}>")));
    assert!(!body.contains(CITY_CLOSED));
}

#[tokio::test]
async fn no_publisher_no_public_endpoint_and_no_project_are_one_404() {
    let realm = common::Realm::new();
    let (_, _, unknown) = get(app_of(&realm, &[]), "/catalog/nobody.jsonld", None).await;
    for refused in [
        "/catalog/bbsk.jsonld",
        "/catalog/quiet.jsonld",
        "/catalog/quiet.ttl",
        "/catalog/nobody.jsonld",
        "/catalog/banskabystrica.xml",
        "/catalog/banskabystrica",
        "/catalog/.jsonld",
        "/catalog/banskabystrica.jsonld?profile=dcat-ap-sk",
        "/catalog/banskabystrica.jsonld?profile=nonsense",
    ] {
        let (status, _, body) = get(app_of(&realm, &[]), refused, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{refused}: {body}");
        assert_eq!(body, unknown, "{refused} says more than an unknown name");
    }
}

#[tokio::test]
async fn a_served_profile_renders_the_catalogue_and_its_records() {
    let realm = common::Realm::new();
    let app = || app_of(&realm, &[DcatProfile::DcatApSk, DcatProfile::DcatApCz]);
    let (_, _, core) = get(app(), "/catalog/banskabystrica.jsonld", None).await;

    let (status, _, sk) = get(
        app(),
        "/catalog/banskabystrica.jsonld?profile=dcat-ap-sk",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{sk}");
    assert_eq!(datasets(&sk), datasets(&core));
    let feed: Value = serde_json::from_str(&sk).expect("JSON-LD");
    assert_eq!(
        feed["dcat:contactPoint"]["vcard:fn"],
        json!([{ "@value": "Oddelenie informatiky", "@language": "sk" }])
    );
    assert!(sk.contains("leg:termsOfUse"), "{sk}");
    assert!(sk.contains("dcat:downloadURL"), "{sk}");

    let (status, _, cz) = get(
        app(),
        "/catalog/banskabystrica.jsonld?profile=dcat-ap-cz",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{cz}");
    let feed: Value = serde_json::from_str(&cz).expect("JSON-LD");
    assert_eq!(
        feed["dct:title"],
        json!({ "@value": "City of Banská Bystrica", "@language": "en" })
    );
    assert!(cz.contains("pu:specifikace"), "{cz}");

    let (status, _, turtle) = get(
        app(),
        "/catalog/banskabystrica.ttl?profile=dcat-ap-sk",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(turtle.contains("\"Oddelenie informatiky\"@sk"), "{turtle}");
}

fn repo(test: &str, files: &[(&str, &str)]) -> PathBuf {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after the epoch")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("gw-publisher-{test}-{now}"));
    std::fs::create_dir_all(&dir).expect("create the repository");
    for (name, body) in files {
        std::fs::write(dir.join(name), body).expect("write a manifest");
    }
    dir
}

const PROJECT: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: Project
metadata:
  name: ovzdusie
  namespace: org
  title: { sk: "Mesto Banská Bystrica", en: "City of Banská Bystrica" }
spec:
  organizationRef: { kind: Organization, name: hel }
  domain: banskabystrica.sk
  catalog:
    publisher:
      name: { sk: "Mesto Banská Bystrica" }
      uri: https://data.gov.sk/id/legal-subject/00313271
    contactPoint: { name: Oddelenie informatiky, email: opendata@example.org }
"#;

const SPACE: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: ContextSpace
metadata:
  name: ovzdusie
  namespace: ovzdusie
spec:
  isSandbox: false
  urnSegment: ovzdusie
"#;

const BARE: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: Endpoint
metadata:
  name: bare
  namespace: ovzdusie
spec:
  contextSpaceRef: ovzdusie
  slug: zt4qm7ge2xdv6ksb3ncf5arw2y
  audience: public
  enabledRepresentations: ["ngsi-ld"]
"#;

const OWN: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: Endpoint
metadata:
  name: own
  namespace: ovzdusie
spec:
  contextSpaceRef: ovzdusie
  slug: h5kq2zt7mwx3vbn6crd4sjf6py
  audience: public
  enabledRepresentations: ["ngsi-ld"]
  catalog:
    publisher: { name: { sk: SHMÚ }, uri: "https://data.gov.sk/id/legal-subject/00156886" }
"#;

fn loaded(dir: &Path) -> (Vec<Endpoint>, Publishers) {
    let tables = store::load(dir).expect("the repository loads");
    (tables.0, tables.10)
}

#[test]
fn the_projects_publisher_reaches_every_record_that_names_none() {
    let dir = repo(
        "fold",
        &[
            ("project.yaml", PROJECT),
            ("space.yaml", SPACE),
            ("bare.yaml", BARE),
            ("own.yaml", OWN),
        ],
    );
    let (endpoints, publishers) = loaded(&dir);
    let published = publishers
        .get("ovzdusie")
        .expect("the project names its publisher");
    assert_eq!(published.domain.as_deref(), Some("banskabystrica.sk"));
    assert_eq!(
        published.title.get("en").map(String::as_str),
        Some("City of Banská Bystrica")
    );

    let catalog_of = |slug: &str| {
        endpoints
            .iter()
            .find(|e| e.slug == slug)
            .and_then(|e| e.catalog.as_deref().cloned())
            .expect("a catalogue block")
    };
    let bare = catalog_of("zt4qm7ge2xdv6ksb3ncf5arw2y");
    assert_eq!(bare.publisher.and_then(|p| p.uri).as_deref(), Some(IRI));
    assert_eq!(
        bare.contact_point.map(|c| c.email).as_deref(),
        Some("opendata@example.org")
    );
    // An Endpoint's own publisher wins; the desk it does not name is the project's.
    let own = catalog_of("h5kq2zt7mwx3vbn6crd4sjf6py");
    assert_eq!(
        own.publisher.and_then(|p| p.uri).as_deref(),
        Some("https://data.gov.sk/id/legal-subject/00156886")
    );
    assert_eq!(
        own.contact_point.map(|c| c.email).as_deref(),
        Some("opendata@example.org")
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_project_without_a_publisher_publishes_no_catalogue_and_changes_no_record() {
    let plain = PROJECT
        .split("  catalog:")
        .next()
        .expect("the head")
        .to_owned();
    let dir = repo(
        "plain",
        &[
            ("project.yaml", &plain),
            ("space.yaml", SPACE),
            ("bare.yaml", BARE),
        ],
    );
    let (endpoints, publishers) = loaded(&dir);
    assert!(publishers.is_empty());
    assert!(endpoints.iter().all(|e| e.catalog.is_none()));
    let _ = std::fs::remove_dir_all(&dir);
}

/// Each national rendering of a publisher's catalogue, written to `DCAT_RECORDS_DIR` (else
/// `target/tmp/dcat-records`) as the conformance repository's `publisher-*.jsonld` fixtures,
/// which its profile suite holds to that profile's shapes (EP-127).
#[tokio::test]
async fn each_national_catalogue_is_written_for_the_profile_check() {
    let directory = std::env::var("DCAT_RECORDS_DIR")
        .unwrap_or_else(|_| format!("{}/dcat-records", env!("CARGO_TARGET_TMPDIR")));
    std::fs::create_dir_all(&directory).expect("the records directory");
    let realm = common::Realm::new();
    for (name, path) in [
        (
            "publisher-sk-banskabystrica",
            "/catalog/banskabystrica.jsonld?profile=dcat-ap-sk",
        ),
        (
            "publisher-cz-praha",
            "/catalog/praha.jsonld?profile=dcat-ap-cz",
        ),
    ] {
        let app = app_of(&realm, &[DcatProfile::DcatApSk, DcatProfile::DcatApCz]);
        let (status, _, body) = get(app, path, None).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let pretty =
            serde_json::to_string_pretty(&serde_json::from_str::<Value>(&body).expect("JSON-LD"))
                .expect("json");
        std::fs::write(format!("{directory}/{name}.jsonld"), pretty).expect("jsonld written");
    }
}
