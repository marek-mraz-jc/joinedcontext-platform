//! T-3052: the crawl worker of `jc-assistant` — reads the organization's `KnowledgeSource`
//! manifests, queues each due website source once, and works a job end to end (claim, crawl,
//! index, done), as the deployment runs it. Needs `JC_ASSISTANT_TEST_DATABASE_URL` (see
//! crawl_tests.rs).

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::Path;
use std::sync::Arc;

use assistant::crawl::{CrawlPolicy, Crawler};
use assistant::worker::{self, Checkout, Source};
use assistant::{project_scope, MIGRATOR};
use jc_core::kinds::assistant::{KnowledgeSourceSpec, PdfPolicy, SourceType, Visibility};
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{ConnectOptions, PgPool};
use time::macros::datetime;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// DNS resolver that maps specific hostnames to local wiremock ports.
#[derive(Clone, Debug)]
struct FixtureResolver {
    mappings: HashMap<String, SocketAddr>,
}

impl FixtureResolver {
    fn new(pairs: &[(&str, u16)]) -> Self {
        let mut mappings = HashMap::new();
        for (host, port) in pairs {
            mappings.insert(
                host.to_string(),
                SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), *port),
            );
        }
        Self { mappings }
    }
}

impl Resolve for FixtureResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_string();
        if let Some(addr) = self.mappings.get(&host) {
            let addrs = vec![*addr];
            Box::pin(std::future::ready(Ok(Box::new(addrs.into_iter()) as Addrs)))
        } else {
            // Hermetic: a name the fixture does not serve never reaches real DNS.
            Box::pin(std::future::ready(Err(format!(
                "the fixture resolver serves no host named {host}"
            )
            .into())))
        }
    }
}

async fn database(test: &str) -> (PgPool, PgPool, String) {
    let url = std::env::var("JC_ASSISTANT_TEST_DATABASE_URL").unwrap_or_else(|_| {
        panic!(
            "set JC_ASSISTANT_TEST_DATABASE_URL to a PostgreSQL with pgvector whose user \
             may create databases (see this file's header)"
        )
    });
    let admin_options: PgConnectOptions =
        url.parse().expect("JC_ASSISTANT_TEST_DATABASE_URL parses");
    let admin = PgPoolOptions::new()
        .max_connections(2)
        .connect_with(admin_options.clone())
        .await
        .expect("the test server answers");
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let name = format!("assistant_crawl_{test}_{}_{nanos}", std::process::id());
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE {name}")))
        .execute(&admin)
        .await
        .expect("create test database");
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect_with(admin_options.database(&name).disable_statement_logging())
        .await
        .expect("connect to test database");
    MIGRATOR.run(&pool).await.expect("migrations apply");
    sqlx::query("DO $$ BEGIN IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = 'assistant_app') THEN CREATE ROLE assistant_app NOLOGIN; END IF; END $$")
        .execute(&pool)
        .await
        .expect("create app role");
    sqlx::query(
        "GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA public TO assistant_app",
    )
    .execute(&pool)
    .await
    .expect("grant table permissions");
    sqlx::query("GRANT USAGE ON ALL SEQUENCES IN SCHEMA public TO assistant_app")
        .execute(&pool)
        .await
        .expect("grant sequence permissions");
    (admin, pool, name)
}

/// Tears down the test database.
async fn drop_database(admin: PgPool, pool: PgPool, name: &str) {
    pool.close().await;
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "DROP DATABASE IF EXISTS {name} WITH (FORCE)"
    )))
    .execute(&admin)
    .await
    .expect("drop test database");
}

fn spec(start: &str, schedule: Option<&str>) -> KnowledgeSourceSpec {
    KnowledgeSourceSpec {
        source: SourceType::Website,
        start_urls: vec![start.to_owned()],
        ckan_instance_ref: None,
        context_spaces: Vec::new(),
        sitemap: false,
        include: Vec::new(),
        exclude: Vec::new(),
        max_depth: 2,
        max_pages: 20,
        pdf: PdfPolicy::default(),
        off_domain_documents: false,
        schedule: schedule.map(str::to_owned),
        languages: Vec::new(),
        visibility: Visibility::Public,
    }
}

fn write(dir: &Path, rel: &str, body: &str) {
    let path = dir.join(rel);
    std::fs::create_dir_all(path.parent().expect("a parent")).expect("mkdir");
    std::fs::write(path, body).expect("write");
}

#[test]
fn a_source_is_due_before_its_first_crawl_then_when_its_schedule_names_the_minute() {
    let nightly = spec("https://x.test/", Some("0 3 * * *"));
    let three = datetime!(2026-10-06 03:00 UTC);
    assert!(
        worker::due(&nightly, None, datetime!(2026-10-06 14:12 UTC)),
        "never read: due at once"
    );
    assert!(worker::due(
        &nightly,
        Some(datetime!(2026-10-05 03:00 UTC)),
        three
    ));
    assert!(!worker::due(
        &nightly,
        Some(datetime!(2026-10-05 03:00 UTC)),
        datetime!(2026-10-06 03:01 UTC)
    ));
    assert!(
        !worker::due(&nightly, Some(three), three),
        "read this minute already"
    );
    let unscheduled = spec("https://x.test/", None);
    assert!(
        worker::due(&unscheduled, Some(datetime!(2026-10-05 03:00 UTC)), three),
        "no schedule reads at 03:00"
    );
    let mut ckan = unscheduled.clone();
    ckan.source = SourceType::Ckan;
    assert!(
        worker::due(&ckan, None, three),
        "a catalogue is read before its first run like a website"
    );
}

#[test]
fn the_sources_are_read_from_the_organizations_checkout_and_a_broken_one_is_left_out() {
    let dir = std::env::temp_dir().join(format!("jc-assistant-worker-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    write(&dir, "projects/ovzdusie/assistant/sources/bb-web.yaml",
        "apiVersion: joinedcontext.com/v1alpha1\nkind: KnowledgeSource\nmetadata:\n  name: bb-web\n  namespace: ovzdusie\nspec:\n  source: website\n  startUrls: [https://www.banskabystrica.sk/]\n  schedule: \"0 3 * * *\"\n");
    write(&dir, "projects/ovzdusie/ckan/data.yaml",
        "apiVersion: joinedcontext.com/v1alpha1\nkind: CkanInstance\nmetadata:\n  name: data\n  namespace: ovzdusie\nspec:\n  url: https://data.banskabystrica.sk\n  apiTokenRef: { name: ckan-api-token, key: token }\n");
    write(&dir, "projects/ovzdusie/assistant/sources/bb-data.yaml",
        "apiVersion: joinedcontext.com/v1alpha1\nkind: KnowledgeSource\nmetadata:\n  name: bb-data\n  namespace: ovzdusie\nspec:\n  source: ckan\n  ckanInstanceRef: data\n");
    write(&dir, "projects/ovzdusie/assistant/sources/bb-other.yaml",
        "apiVersion: joinedcontext.com/v1alpha1\nkind: KnowledgeSource\nmetadata:\n  name: bb-other\n  namespace: ovzdusie\nspec:\n  source: ckan\n  ckanInstanceRef: undeclared\n");
    write(&dir, "projects/ovzdusie/assistant/sources/broken.yaml",
        "apiVersion: joinedcontext.com/v1alpha1\nkind: KnowledgeSource\nmetadata:\n  name: broken\n  namespace: ovzdusie\nspec:\n  source: website\n  startUrls: 7\n");
    let found = worker::sources(&Checkout {
        organization: dir.clone(),
        projects: None,
        assembly: dir.join(".assembly"),
    })
    .expect("the checkout reads");
    let names: Vec<(&str, &str)> = found
        .iter()
        .map(|s| (s.project.as_str(), s.name.as_str()))
        .collect();
    assert_eq!(
        names,
        [
            ("ovzdusie", "bb-data"),
            ("ovzdusie", "bb-other"),
            ("ovzdusie", "bb-web")
        ]
    );
    let url = |name: &str| {
        found
            .iter()
            .find(|s| s.name == name)
            .and_then(|s| s.ckan_url.clone())
    };
    assert_eq!(
        url("bb-data").as_deref(),
        Some("https://data.banskabystrica.sk")
    );
    assert_eq!(
        url("bb-other"),
        None,
        "an instance the project does not declare"
    );
    assert_eq!(url("bb-web"), None);
    assert_eq!(found[2].spec.schedule.as_deref(), Some("0 3 * * *"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_due_source_is_queued_once_and_its_job_crawls_and_indexes_the_site() {
    let (admin, pool, name) = database("worker").await;
    let server = MockServer::start().await;
    let port = server.address().port();
    Mock::given(method("GET"))
        .and(path("/robots.txt"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    Mock::given(method("GET")).and(path("/"))
        .respond_with(ResponseTemplate::new(200).insert_header("content-type", "text/html; charset=utf-8")
            .set_body_string("<html lang=\"sk\"><head><title>Mesto</title></head><body><h1>Odpad</h1><p>Zber odpadu je každý utorok v celom meste a obyvatelia vynášajú nádoby večer pred zberom.</p></body></html>"))
        .mount(&server).await;
    let crawler = Crawler::new(
        Arc::new(FixtureResolver::new(&[("www.city.test", port)])),
        CrawlPolicy {
            allow_plain_http: true,
            max_page_bytes: 1024 * 1024,
        },
    )
    .expect("crawler");
    let sources = vec![Source {
        project: "helsinki".into(),
        name: "city-web".into(),
        spec: spec(&format!("http://www.city.test:{port}/"), Some("0 3 * * *")),
        ckan_url: None,
        catalogue: Vec::new(),
    }];
    let now = datetime!(2026-10-06 14:12 UTC);

    assert_eq!(
        worker::enqueue_due(&pool, &sources, now)
            .await
            .expect("queued"),
        1
    );
    assert_eq!(
        worker::enqueue_due(&pool, &sources, now)
            .await
            .expect("queued"),
        0,
        "no second job while one waits"
    );

    assert!(
        worker::work_one(&pool, &crawler, &reader(), "hel.fi", "worker-a", &sources)
            .await
            .expect("worked")
    );
    assert!(
        !worker::work_one(&pool, &crawler, &reader(), "hel.fi", "worker-a", &sources)
            .await
            .expect("nothing left")
    );

    let state: String = sqlx::query_scalar("SELECT state FROM crawl_jobs")
        .fetch_one(&pool)
        .await
        .expect("job");
    assert_eq!(state, "done");
    let mut tx = project_scope(&pool, "helsinki").await.expect("scope");
    let chunks: i64 = sqlx::query_scalar("SELECT count(*) FROM chunks")
        .fetch_one(&mut *tx)
        .await
        .expect("chunks");
    tx.rollback().await.expect("rollback");
    assert!(chunks >= 1, "the page was indexed");

    // Read today, not due again until its schedule names a minute.
    assert_eq!(
        worker::enqueue_due(&pool, &sources, now + time::Duration::minutes(5))
            .await
            .expect("queued"),
        0
    );
    drop_database(admin, pool, &name).await;
}

#[tokio::test]
async fn a_job_whose_source_went_away_is_closed_without_a_crawl() {
    let (admin, pool, name) = database("gone").await;
    assistant::crawl::queue::enqueue(&pool, "helsinki", "removed")
        .await
        .expect("queued");
    let crawler =
        Crawler::new(Arc::new(FixtureResolver::new(&[])), CrawlPolicy::default()).expect("crawler");
    assert!(
        worker::work_one(&pool, &crawler, &reader(), "hel.fi", "worker-a", &[])
            .await
            .expect("worked")
    );
    let state: String = sqlx::query_scalar("SELECT state FROM crawl_jobs")
        .fetch_one(&pool)
        .await
        .expect("job");
    assert_eq!(state, "done");
    drop_database(admin, pool, &name).await;
}

#[tokio::test]
async fn a_catalogue_job_indexes_its_datasets_and_one_naming_no_instance_fails_with_why() {
    let (admin, pool, name) = database("catalogue").await;
    let server = MockServer::start().await;
    let port = server.address().port();
    Mock::given(method("GET"))
        .and(path("/api/3/action/package_search"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "success": true,
            "result": {"count": 1, "results": [{
                "name": "ovzdusie-merania",
                "title": "Merania kvality ovzdušia",
                "notes": "Hodinové merania oxidu dusičitého a prachových častíc PM10 z piatich staníc v meste.",
                "metadata_modified": "2026-10-06T08:00:00",
                "resources": [{"name": "Merania", "format": "CSV"}]
            }]}
        })))
        .mount(&server)
        .await;
    let crawler = Crawler::new(
        Arc::new(FixtureResolver::new(&[("data.city.test", port)])),
        CrawlPolicy {
            allow_plain_http: true,
            max_page_bytes: 1024 * 1024,
        },
    )
    .expect("crawler");
    let mut catalogue = spec("https://unused.test/", None);
    catalogue.source = SourceType::Ckan;
    catalogue.start_urls.clear();
    catalogue.ckan_instance_ref = Some("data".into());
    catalogue.languages = vec!["sk".into()];
    let sources = vec![
        Source {
            project: "helsinki".into(),
            name: "catalogue".into(),
            spec: catalogue.clone(),
            ckan_url: Some(format!("http://data.city.test:{port}")),
            catalogue: Vec::new(),
        },
        Source {
            project: "helsinki".into(),
            name: "orphan".into(),
            spec: catalogue,
            ckan_url: None,
            catalogue: Vec::new(),
        },
    ];
    let now = datetime!(2026-10-06 14:12 UTC);
    assert_eq!(
        worker::enqueue_due(&pool, &sources, now)
            .await
            .expect("queued"),
        2
    );
    for _ in 0..2 {
        assert!(
            worker::work_one(&pool, &crawler, &reader(), "hel.fi", "worker-a", &sources)
                .await
                .expect("worked")
        );
    }
    let jobs: Vec<(String, String, Option<String>)> =
        sqlx::query_as("SELECT source, state, error FROM crawl_jobs ORDER BY source")
            .fetch_all(&pool)
            .await
            .expect("jobs");
    assert_eq!(jobs[0].0, "catalogue");
    assert_eq!(jobs[0].1, "done");
    assert_eq!(jobs[1].0, "orphan");
    assert_ne!(jobs[1].1, "done");
    let why = jobs[1].2.clone().unwrap_or_default();
    assert!(
        why.contains("CkanInstance `data`") && why.contains("does not declare"),
        "{why}"
    );

    let mut tx = project_scope(&pool, "helsinki").await.expect("scope");
    let cited: Vec<String> = sqlx::query_scalar("SELECT DISTINCT url FROM chunks")
        .fetch_all(&mut *tx)
        .await
        .expect("chunks");
    tx.rollback().await.expect("rollback");
    assert_eq!(
        cited,
        [format!(
            "http://data.city.test:{port}/dataset/ovzdusie-merania"
        )]
    );
    drop_database(admin, pool, &name).await;
}

/// No gateway and no public Endpoint address: what a test of the other sources needs.
fn reader() -> assistant::catalogue::Reader {
    assistant::catalogue::Reader {
        http: reqwest::Client::new(),
        gateway: None,
        endpoint_base: None,
    }
}
