//! A `catalogue` knowledge source on a seeded project (T-3225, AG-116): the pages come from the
//! repository, a public source holds public Endpoints alone, only a public Endpoint is counted,
//! and each page cites its CKAN dataset or the Endpoint's public address. The indexing test
//! needs the test database (see `common`).

#[path = "common/db.rs"]
mod db;

use std::path::Path;
use std::time::Duration;

use assistant::catalogue::{sync_catalogue, EndpointPage, Held, Reader};
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
    write(&dir, "projects/bb/project.yaml",
        "apiVersion: joinedcontext.com/v1alpha1\nkind: Project\nmetadata:\n  name: bb\n  namespace: org\n  title: { sk: Mesto Banská Bystrica, en: City of Banská Bystrica }\nspec:\n  organizationRef: org\n");
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
        // Once: the internal source holds the public Endpoint too and reads its held counts.
        .expect(1)
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
        .expect(1)
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
        portal: None,
        held: Default::default(),
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

/// One chunk of a CKAN dataset page of `project`'s site `site`, at `visibility`.
async fn dataset_chunk(pool: &sqlx::PgPool, project: &str, site: i64, url: &str, text: &str) {
    let mut tx = assistant::project_scope(pool, project)
        .await
        .expect("scope");
    let visibility: String = sqlx::query_scalar("SELECT visibility FROM sites WHERE id = $1")
        .bind(site)
        .fetch_one(&mut *tx)
        .await
        .expect("site");
    let page: i64 = sqlx::query_scalar(
        "INSERT INTO pages (site_id, project, url, depth) VALUES ($1, $2, $3, 0) RETURNING id",
    )
    .bind(site)
    .bind(project)
    .bind(url)
    .fetch_one(&mut *tx)
    .await
    .expect("page");
    sqlx::query(
        "INSERT INTO chunks (site_id, project, page_id, ordinal, url, text, lang, visibility) \
         VALUES ($1, $2, $3, 0, $4, $5, 'sk', $6)",
    )
    .bind(site)
    .bind(project)
    .bind(page)
    .bind(url)
    .bind(text)
    .bind(visibility)
    .execute(&mut *tx)
    .await
    .expect("chunk");
    tx.commit().await.expect("commit");
}

/// T-4793: an Endpoint with no dataset of its own whose type one CKAN dataset of the project
/// publishes (named by its OData address, as a harvested dataset names it): the type's passage
/// is indexed at that dataset's page, and the Endpoint's page names it. An internal dataset is
/// never named for a public Endpoint.
#[tokio::test]
async fn a_type_one_dataset_publishes_is_indexed_and_cited_at_that_dataset() {
    let (admin, pool, name) = db::database("catalogue_type_datasets").await;
    let reader = Reader {
        http: reqwest::Client::new(),
        gateway: None,
        endpoint_base: Url::parse("https://platform.example.org/").ok(),
        portal: None,
        held: Default::default(),
    };
    let ckan_public = upsert_site(&pool, "bb", "data.example.org", "ckan", Visibility::Public)
        .await
        .expect("site");
    let ckan_internal = upsert_site(
        &pool,
        "bb",
        "data.example.org",
        "ckan-staff",
        Visibility::Internal,
    )
    .await
    .expect("site");
    let stations = "https://data.example.org/dataset/stanice-ovzdusia";
    dataset_chunk(&pool, "bb", ckan_public, stations, &format!(
        "# Stanice ovzdušia\n\nOData: https://platform.example.org/api/endpoint/{PRIVATE_SLUG}/odata/v4/AirQualityStation"
    )).await;
    let internal = "https://data.example.org/dataset/merania-interne";
    dataset_chunk(&pool, "bb", ckan_internal, internal, &format!(
        "# Merania interné\n\nhttps://platform.example.org/api/endpoint/{PRIVATE_SLUG}/ngsi-ld/v1/entities?type=AirQualityObserved"
    )).await;
    // A page that names the address but is no dataset page is never a type's dataset.
    dataset_chunk(
        &pool,
        "bb",
        ckan_public,
        "https://data.example.org/news/1",
        &format!("# News\n\n/api/endpoint/{PRIVATE_SLUG}/odata/v4/AirQualityStation"),
    )
    .await;

    let sources = worker::sources(&checkout("type_datasets")).expect("reads");
    let source = source_owned(&sources, "staff");
    let site = upsert_site(
        &pool,
        "bb",
        "example.org",
        &source.name,
        Visibility::Internal,
    )
    .await
    .expect("site");
    let mut pages = Pages::default();
    sync_catalogue(
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
    let html = |url: &str| &pages.0.iter().find(|(u, _)| u == url).expect(url).1;
    let own = html(&format!(
        "https://platform.example.org/api/endpoint/{PRIVATE_SLUG}/schema/index.json"
    ));
    assert!(
        own.contains(&format!(
            "<li>Type AirQualityStation: Stanice ovzdušia ({stations})</li>"
        )),
        "{own}"
    );
    assert!(
        own.contains(&format!(
            "<li>Type AirQualityObserved: Merania interné ({internal})</li>"
        )),
        "an internal Endpoint names its internal dataset: {own}"
    );
    assert!(
        !own.contains("<h2>AirQualityStation</h2>"),
        "the type moved: {own}"
    );
    let station = html(stations);
    assert!(station.contains("<h1>Stanice ovzdušia</h1>"), "{station}");
    assert!(station.contains("<h2>AirQualityStation</h2>"));
    assert!(!station.contains("<h2>AirQualityObserved</h2>"));
    assert!(pages.0.iter().all(|(u, _)| !u.contains("/news/")));

    // A public Endpoint reads public dataset pages alone.
    let kinds = [
        "AirQualityObserved".to_owned(),
        "AirQualityStation".to_owned(),
    ];
    let mut tx = assistant::project_scope(&pool, "bb").await.expect("scope");
    let public = assistant::type_datasets(&mut tx, PRIVATE_SLUG, &kinds, site, true)
        .await
        .expect("read");
    tx.rollback().await.expect("rollback");
    let urls: Vec<&str> = public.iter().map(|d| d.url.as_str()).collect();
    assert_eq!(urls, [stations]);
    db::drop_database(admin, pool, &name).await;
}

/// T-5368: the hub's document search reads a question's publisher by the project's own title,
/// in every language the Project manifest gives it.
#[test]
fn each_project_is_named_by_its_title_in_every_language() {
    let snapshot = worker::snapshot(&checkout("titles")).expect("reads");
    assert_eq!(
        snapshot.titles.get("bb").map(Vec::as_slice),
        Some(
            &[
                "City of Banská Bystrica".to_owned(),
                "Mesto Banská Bystrica".to_owned()
            ][..]
        )
    );
}

/// A count of the public Endpoint of `checkout("index")` answering `count`, or `status` alone.
fn count_answer(class: &str, status: u16, count: Option<&str>) -> Mock {
    let mut answer = ResponseTemplate::new(status).set_body_string("[]");
    if let Some(count) = count {
        answer = answer.insert_header("NGSILD-Results-Count", count);
    }
    Mock::given(method("GET"))
        .and(path(format!(
            "/api/endpoint/{PUBLIC_SLUG}/ngsi-ld/v1/entities"
        )))
        .and(query_param("type", class))
        .and(query_param("count", "true"))
        .respond_with(answer)
}

/// One sync of `pages` into a fresh site of project bb; the pages written.
async fn sync_once(
    pool: &sqlx::PgPool,
    reader: &Reader,
    site: i64,
    pages: &[EndpointPage],
) -> Pages {
    let mut written = Pages::default();
    sync_catalogue(pool, reader, "bb", site, pages, Some("sk"), &mut written)
        .await
        .expect("indexed");
    written
}

fn air(pages: &Pages) -> &str {
    &pages
        .0
        .iter()
        .find(|(url, _)| url == "https://data.example.org/dataset/kvalita-ovzdusia")
        .expect("the air page")
        .1
}

/// T-5795: a gateway that sheds the second count of an Endpoint is asked no third time, and the
/// page keeps the last good counts with the time they were counted.
#[tokio::test]
async fn a_refused_count_is_not_asked_again_and_the_last_good_counts_stand() {
    let (admin, pool, name) = db::database("catalogue_shed").await;
    let gateway = MockServer::start().await;
    count_answer("AirQualityObserved", 200, Some("14"))
        .up_to_n_times(1)
        .expect(1)
        .mount(&gateway)
        .await;
    count_answer("AirQualityStation", 200, Some("0"))
        .up_to_n_times(1)
        .expect(1)
        .mount(&gateway)
        .await;
    // The second sync: its first count comes back, the second is shed, and nothing more is asked.
    count_answer("AirQualityObserved", 200, Some("15"))
        .expect(1)
        .mount(&gateway)
        .await;
    count_answer("AirQualityStation", 503, None)
        .expect(1)
        .mount(&gateway)
        .await;
    let reader = Reader {
        http: reqwest::Client::new(),
        gateway: Url::parse(&format!("{}/", gateway.uri())).ok(),
        endpoint_base: Url::parse("https://platform.example.org/").ok(),
        portal: None,
        // Every sync asks again: only a refusal keeps the last counts.
        held: Held::new(Duration::ZERO),
    };
    let sources = worker::sources(&checkout("shed")).expect("reads");
    let source = source_owned(&sources, "catalogue");
    let site = upsert_site(&pool, "bb", "example.org", &source.name, Visibility::Public)
        .await
        .expect("site");
    let first = sync_once(&pool, &reader, site, &source.catalogue).await;
    let second = sync_once(&pool, &reader, site, &source.catalogue).await;
    let counted = |html: &str| {
        html.split("14 entities of type AirQualityObserved (counted ")
            .nth(1)
            .and_then(|rest| rest.split(')').next())
            .map(str::to_owned)
    };
    assert!(counted(air(&first)).is_some(), "{}", air(&first));
    assert_eq!(
        counted(air(&second)),
        counted(air(&first)),
        "{}",
        air(&second)
    );
    assert!(
        !air(&second).contains("AirQualityStation"),
        "the held zero still hides it"
    );
    gateway.verify().await;
    db::drop_database(admin, pool, &name).await;
}

/// T-5795: a sync whose Endpoint did not change sends no count read; its page keeps the counts.
/// One whose types changed is counted again, a title change is not.
#[tokio::test]
async fn an_unchanged_endpoint_is_not_counted_again() {
    let (admin, pool, name) = db::database("catalogue_unchanged").await;
    let gateway = MockServer::start().await;
    count_answer("AirQualityObserved", 200, Some("14"))
        .expect(2)
        .mount(&gateway)
        .await;
    count_answer("AirQualityStation", 200, Some("0"))
        .expect(1)
        .mount(&gateway)
        .await;
    let reader = Reader {
        http: reqwest::Client::new(),
        gateway: Url::parse(&format!("{}/", gateway.uri())).ok(),
        endpoint_base: Url::parse("https://platform.example.org/").ok(),
        portal: None,
        held: Held::default(),
    };
    let sources = worker::sources(&checkout("unchanged")).expect("reads");
    let source = source_owned(&sources, "catalogue");
    let site = upsert_site(&pool, "bb", "example.org", &source.name, Visibility::Public)
        .await
        .expect("site");
    let first = sync_once(&pool, &reader, site, &source.catalogue).await;
    let second = sync_once(&pool, &reader, site, &source.catalogue).await;
    assert_eq!(air(&second), air(&first), "the held counts, the same page");
    let mut changed = source.catalogue.clone();
    for page in &mut changed {
        page.title = Some("Ovzdušie".into());
    }
    sync_once(&pool, &reader, site, &changed).await;
    for page in &mut changed {
        page.types.retain(|t| t.class == "AirQualityObserved");
    }
    sync_once(&pool, &reader, site, &changed).await;
    gateway.verify().await;
    db::drop_database(admin, pool, &name).await;
}

/// T-5795: a gateway shedding every count gets three refusals in one sync, not one per Endpoint.
#[tokio::test]
async fn a_shedding_gateway_gets_a_few_refusals_per_sync() {
    let (admin, pool, name) = db::database("catalogue_stall").await;
    let gateway = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(503))
        .expect(3)
        .mount(&gateway)
        .await;
    let reader = Reader {
        http: reqwest::Client::new(),
        gateway: Url::parse(&format!("{}/", gateway.uri())).ok(),
        endpoint_base: Url::parse("https://platform.example.org/").ok(),
        portal: None,
        held: Held::default(),
    };
    let sources = worker::sources(&checkout("stall")).expect("reads");
    let source = source_owned(&sources, "catalogue");
    let public = source
        .catalogue
        .iter()
        .find(|page| page.slug == PUBLIC_SLUG)
        .expect("the counted Endpoint");
    let many: Vec<EndpointPage> = (0..6)
        .map(|n| EndpointPage {
            slug: format!("{}{n}", &public.slug[..25]),
            ckan_dataset: Some(format!("https://data.example.org/dataset/air-{n}")),
            ..public.clone()
        })
        .collect();
    let site = upsert_site(&pool, "bb", "example.org", &source.name, Visibility::Public)
        .await
        .expect("site");
    let written = sync_once(&pool, &reader, site, &many).await;
    assert_eq!(written.0.len(), 6, "every page is indexed, uncounted");
    for (_, html) in &written.0 {
        assert!(html.contains("AirQualityStation"), "{html}");
    }
    gateway.verify().await;
    db::drop_database(admin, pool, &name).await;
}
