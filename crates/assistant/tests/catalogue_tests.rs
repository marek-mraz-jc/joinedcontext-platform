//! A `catalogue` knowledge source on a seeded project (T-3225, AG-116): the pages come from the
//! repository, a public source holds public Endpoints alone, only a public Endpoint is counted,
//! and each page cites its CKAN dataset or the Endpoint's public address. The indexing test
//! needs the test database (see `common`).

#[path = "common/db.rs"]
mod db;

use std::path::Path;

use assistant::catalogue::{sync_catalogue, Reader};
use assistant::crawl::{upsert_site, Sink};
use assistant::worker::{self, Checkout, Source};
use jc_core::kinds::assistant::Visibility;
use url::Url;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const PUBLIC_SLUG: &str = "k7m2qz4tv6xh3n5jb2ryd3wcfa";
const PRIVATE_SLUG: &str = "p3n5jb2ryd3wcfak7m2qz4tv6x";

fn write(dir: &Path, rel: &str, body: &str) {
    let path = dir.join(rel);
    std::fs::create_dir_all(path.parent().expect("a parent")).expect("mkdir");
    std::fs::write(path, body).expect("write");
}

/// Project `bb`: space `ovzdusie` with its model, a public Endpoint published to CKAN that hides
/// one attribute, and an organization-only Endpoint over the same space; space `mesto` with a
/// public Endpoint; a public catalogue source over every space and an internal one over
/// `ovzdusie`.
fn checkout(test: &str) -> Checkout {
    let dir = std::env::temp_dir().join(format!(
        "jc-assistant-catalogue-{test}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    let manifest = |kind: &str, name: &str, spec: &str| {
        format!(
            "apiVersion: joinedcontext.com/v1alpha1\nkind: {kind}\nmetadata:\n  name: {name}\n  namespace: bb\n  title: {{ sk: \"Ovzdušie {name}\", en: \"Air {name}\" }}\nspec:\n{spec}"
        )
    };
    let endpoint = |slug: &str, space: &str, audience: &str, extra: &str| {
        format!("  contextSpaceRef: {space}\n  slug: {slug}\n  audience: {audience}\n  enabledRepresentations: [ngsi-ld]\n{extra}")
    };
    write(&dir, "projects/bb/spaces/ovzdusie/datamodels/air.yaml", &manifest("DataModel", "air",
        "  contextSpaceRef: ovzdusie\n  linkml: ./air.linkml.yaml\n  version: 1.0.0\n  lifecycle: draft\n  classes: [AirQualityObserved, AirQualityStation]\n  artifacts:\n    jsonSchema: ./air.v1.schema.json\n"));
    write(&dir, "projects/bb/spaces/ovzdusie/datamodels/air.v1.schema.json", &serde_json::json!({
        "definitions": { "AirQualityObserved": {
            "description": "One air-quality reading of one station.",
            "properties": {
                "id": { "type": "string" }, "type": { "type": "string" },
                "pm10": { "description": "Particulate matter up to 10 µm.", "x-ngsi-ld-kind": "Property", "x-unit": { "ucumCode": "ug/m3" } },
                "stationNote": { "description": "An internal remark of the station's keeper.", "x-ngsi-ld-kind": "Property" }
            }
        },
        "AirQualityStation": {
            "description": "A station the city plans.",
            "properties": { "id": { "type": "string" }, "type": { "type": "string" }, "plannedFor": { "x-ngsi-ld-kind": "Property" } }
        }}
    }).to_string());
    write(&dir, "projects/bb/spaces/ovzdusie/endpoints/public-air.yaml", &manifest("Endpoint", "public-air",
        &endpoint(PUBLIC_SLUG, "ovzdusie", "public",
            "  projection: { hiddenAttributes: [stationNote] }\n  publish:\n    ckan: { instanceRef: data, name: kvalita-ovzdusia }\n")));
    write(
        &dir,
        "projects/bb/spaces/ovzdusie/endpoints/staff-air.yaml",
        &manifest(
            "Endpoint",
            "staff-air",
            &endpoint(PRIVATE_SLUG, "ovzdusie", "organization", ""),
        ),
    );
    write(
        &dir,
        "projects/bb/spaces/mesto/endpoints/mesto-all.yaml",
        &manifest(
            "Endpoint",
            "mesto-all",
            &endpoint("m2qz4tv6xh3n5jb2ryd3wcfak7", "mesto", "public", ""),
        ),
    );
    write(&dir, "projects/bb/spaces/ovzdusie/projections/pm10-only.yaml",
        "apiVersion: joinedcontext.com/v1alpha1\nkind: ModelProjection\nmetadata:\n  name: pm10-only\n  namespace: bb\nspec:\n  contextSpaceRef: ovzdusie\n  dataModelRef: { name: air, version: \"1\" }\n  classes:\n    - { name: AirQualityObserved, slots: [pm10] }\n");
    write(
        &dir,
        "projects/bb/spaces/ovzdusie/endpoints/air-app.yaml",
        &manifest(
            "Endpoint",
            "air-app",
            &endpoint(
                "a7m2qz4tv6xh3n5jb2ryd3wcfk",
                "ovzdusie",
                "organization",
                "  projectionRef: { kind: ModelProjection, name: pm10-only }\n",
            ),
        ),
    );
    write(&dir, "projects/bb/ckan/data.yaml",
        "apiVersion: joinedcontext.com/v1alpha1\nkind: CkanInstance\nmetadata:\n  name: data\n  namespace: bb\nspec:\n  url: https://data.example.org\n  apiTokenRef: { name: ckan-api-token, key: token }\n");
    write(&dir, "projects/bb/assistant/sources/catalogue.yaml",
        "apiVersion: joinedcontext.com/v1alpha1\nkind: KnowledgeSource\nmetadata:\n  name: catalogue\n  namespace: bb\nspec:\n  source: catalogue\n  visibility: public\n  languages: [sk]\n");
    write(&dir, "projects/bb/assistant/sources/staff.yaml",
        "apiVersion: joinedcontext.com/v1alpha1\nkind: KnowledgeSource\nmetadata:\n  name: staff\n  namespace: bb\nspec:\n  source: catalogue\n  visibility: internal\n  contextSpaces: [ovzdusie]\n");
    Checkout {
        organization: dir.clone(),
        projects: None,
        assembly: dir.join(".assembly"),
    }
}

fn source<'a>(sources: &'a [Source], name: &str) -> &'a Source {
    sources.iter().find(|s| s.name == name).expect(name)
}

#[test]
fn a_public_catalogue_holds_public_endpoints_alone_and_an_internal_one_its_spaces() {
    let sources = worker::sources(&checkout("pages")).expect("the checkout reads");

    let public = source(&sources, "catalogue");
    let names: Vec<&str> = public.catalogue.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(
        names,
        ["mesto-all", "public-air"],
        "the organization's Endpoint never reaches a public source"
    );
    let air = public
        .catalogue
        .iter()
        .find(|p| p.name == "public-air")
        .expect("public-air");
    assert_eq!(
        air.title.as_deref(),
        Some("Ovzdušie public-air"),
        "the source's language first"
    );
    assert_eq!(
        air.ckan_dataset.as_deref(),
        Some("https://data.example.org/dataset/kvalita-ovzdusia")
    );
    let attributes: Vec<&str> = air.types[0]
        .attributes
        .iter()
        .map(|a| a.name.as_str())
        .collect();
    assert_eq!(
        attributes,
        ["pm10"],
        "a hidden attribute is not described: the Endpoint never serves it"
    );
    let mesto = public
        .catalogue
        .iter()
        .find(|p| p.name == "mesto-all")
        .expect("mesto-all");
    assert!(
        mesto.types.is_empty(),
        "a space with no model has an Endpoint and no types"
    );

    let staff = source(&sources, "staff");
    let names: Vec<&str> = staff.catalogue.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(
        names,
        ["air-app", "public-air", "staff-air"],
        "every Endpoint of the named space"
    );
    let app = staff
        .catalogue
        .iter()
        .find(|p| p.name == "air-app")
        .expect("air-app");
    let attributes: Vec<&str> = app.types[0]
        .attributes
        .iter()
        .map(|a| a.name.as_str())
        .collect();
    assert_eq!(
        attributes,
        ["pm10"],
        "a ModelProjection narrows the page to its slots"
    );
    let private = staff
        .catalogue
        .iter()
        .find(|p| p.name == "staff-air")
        .expect("staff-air");
    assert_eq!(
        private.types[0].attributes.len(),
        2,
        "no projection, every attribute"
    );
}

#[test]
fn a_change_to_an_endpoint_changes_the_digest_the_worker_compares() {
    let checkout = checkout("digest");
    let before = worker::sources(&checkout).expect("reads");
    let digest =
        |sources: &[Source]| assistant::catalogue::digest(&source(sources, "catalogue").catalogue);
    write(&checkout.organization, "projects/bb/spaces/mesto/endpoints/mesto-all.yaml",
        "apiVersion: joinedcontext.com/v1alpha1\nkind: Endpoint\nmetadata:\n  name: mesto-all\n  namespace: bb\n  title: Mesto, renamed\nspec:\n  contextSpaceRef: mesto\n  slug: m2qz4tv6xh3n5jb2ryd3wcfak7\n  audience: public\n  enabledRepresentations: [ngsi-ld]\n");
    let after = worker::sources(&checkout).expect("reads");
    assert_ne!(digest(&before), digest(&after));
    assert_eq!(
        digest(&after),
        digest(&worker::sources(&checkout).expect("reads"))
    );
}

#[derive(Default)]
struct Pages(Vec<(String, String)>);

impl Sink for Pages {
    async fn page(&mut self, _id: i64, url: &str, _language: Option<&str>, html: &str) {
        self.0.push((url.to_owned(), html.to_owned()));
    }
    async fn document(&mut self, _id: i64, _url: &str, _mime: Option<&str>, _bytes: &[u8]) {}
}

#[tokio::test]
async fn a_public_endpoint_is_counted_anonymously_and_each_page_cites_where_a_visitor_reads_it() {
    let (admin, pool, name) = db::database("catalogue").await;
    let gateway = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!(
            "/api/endpoint/{PUBLIC_SLUG}/ngsi-ld/v1/entities"
        )))
        .and(query_param("type", "AirQualityObserved"))
        .and(query_param("count", "true"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("NGSILD-Results-Count", "14")
                .set_body_string("[]"),
        )
        // Once per source: the internal one holds the public Endpoint too.
        .expect(2)
        .mount(&gateway)
        .await;
    // A type a visitor finds no entity of is left off the public page.
    Mock::given(method("GET"))
        .and(path(format!(
            "/api/endpoint/{PUBLIC_SLUG}/ngsi-ld/v1/entities"
        )))
        .and(query_param("type", "AirQualityStation"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("NGSILD-Results-Count", "0")
                .set_body_string("[]"),
        )
        .expect(2)
        .mount(&gateway)
        .await;
    // The organization's Endpoint is never asked: its rows are not a visitor's to count.
    Mock::given(path(format!(
        "/api/endpoint/{PRIVATE_SLUG}/ngsi-ld/v1/entities"
    )))
    .respond_with(ResponseTemplate::new(200).insert_header("NGSILD-Results-Count", "99"))
    .expect(0)
    .mount(&gateway)
    .await;
    let reader = Reader {
        http: reqwest::Client::new(),
        gateway: Url::parse(&format!("{}/", gateway.uri())).ok(),
        endpoint_base: Url::parse("https://platform.example.org/").ok(),
    };
    let sources = worker::sources(&checkout("index")).expect("reads");

    for (source, visibility) in [
        ("catalogue", Visibility::Public),
        ("staff", Visibility::Internal),
    ] {
        let source = source_owned(&sources, source);
        let site = upsert_site(&pool, "bb", "example.org", &source.name, visibility)
            .await
            .expect("site");
        let mut pages = Pages::default();
        let report = sync_catalogue(
            &pool,
            &reader,
            "bb",
            site,
            &source.catalogue,
            Some("sk"),
            &mut pages,
        )
        .await
        .expect("indexed");
        let urls: Vec<&str> = pages.0.iter().map(|(url, _)| url.as_str()).collect();
        let html = |url: &str| &pages.0.iter().find(|(u, _)| u == url).expect(url).1;
        if source.name == "catalogue" {
            assert_eq!(report.endpoints, 2);
            assert_eq!(urls, [
                "https://platform.example.org/api/endpoint/m2qz4tv6xh3n5jb2ryd3wcfak7/schema/index.json",
                "https://data.example.org/dataset/kvalita-ovzdusia",
            ]);
            let air = html("https://data.example.org/dataset/kvalita-ovzdusia");
            assert!(
                air.contains("14 entities of type AirQualityObserved (counted 20"),
                "{air}"
            );
            assert!(air.contains("pm10 (Property), unit ug/m3"));
            assert!(
                !air.contains("AirQualityStation"),
                "no entity of it reaches a visitor: {air}"
            );
            for secret in [PRIVATE_SLUG, "staff-air", "stationNote"] {
                assert!(
                    pages.0.iter().all(|(_, h)| !h.contains(secret)),
                    "{secret} reached a public page"
                );
            }
        } else {
            let staff = html(&format!(
                "https://platform.example.org/api/endpoint/{PRIVATE_SLUG}/schema/index.json"
            ));
            assert!(staff.contains("audience organization"));
            assert!(
                staff.contains("AirQualityStation"),
                "not counted, so every type of the model"
            );
            assert!(
                !staff.contains("entities of type"),
                "a private Endpoint is not counted"
            );
        }
    }
    gateway.verify().await;
    db::drop_database(admin, pool, &name).await;
}

fn source_owned(sources: &[Source], name: &str) -> Source {
    source(sources, name).clone()
}
