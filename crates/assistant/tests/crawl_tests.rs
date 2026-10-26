//! Tests for the knowledge assistant website crawler (T-3052).
//!
//! Requires a running PostgreSQL instance with pgvector whose user may create databases,
//! configured via the `JC_ASSISTANT_TEST_DATABASE_URL` environment variable:
//!
//! ```text
//! docker run -d -p 5433:5432 -e POSTGRES_PASSWORD=pw ghcr.io/cloudnative-pg/postgis:16-3.6-system-trixie
//! JC_ASSISTANT_TEST_DATABASE_URL=postgres://postgres:pw@127.0.0.1:5433/postgres cargo test -p assistant --test crawl_tests
//! ```

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use agent_proxy::public_dns::{refused, PublicOnly};
use assistant::crawl::fetch;
use assistant::crawl::patterns;
use assistant::crawl::queue;
use assistant::crawl::robots;
use assistant::crawl::wordpress;
use assistant::crawl::{crawl_site, CrawlPolicy, Crawler, Sink};
use assistant::{project_scope, MIGRATOR};
use jc_core::kinds::assistant::{
    KnowledgeSourceSpec, PdfPolicy, PdfPolicyKind, SourceType, Visibility,
};
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{ConnectOptions, PgPool, Row};
use wiremock::matchers::{method, path, query_param};
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

/// Sink implementation that records page and document payloads delivered by the crawler.
#[derive(Default)]
struct RecordingSink {
    pages: Vec<(i64, String, Option<String>, String)>,
    documents: Vec<(i64, String, Option<String>, Vec<u8>)>,
}

impl Sink for RecordingSink {
    fn page(
        &mut self,
        page_id: i64,
        url: &str,
        language: Option<&str>,
        html: &str,
    ) -> impl std::future::Future<Output = ()> + Send {
        self.pages.push((
            page_id,
            url.to_string(),
            language.map(String::from),
            html.to_string(),
        ));
        async {}
    }

    fn document(
        &mut self,
        document_id: i64,
        url: &str,
        mime: Option<&str>,
        bytes: &[u8],
    ) -> impl std::future::Future<Output = ()> + Send {
        self.documents.push((
            document_id,
            url.to_string(),
            mime.map(String::from),
            bytes.to_vec(),
        ));
        async {}
    }
}

/// Creates a fresh database for an isolated test run and applies store migrations.
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

fn test_spec(start_url: &str) -> KnowledgeSourceSpec {
    KnowledgeSourceSpec {
        source: SourceType::Website,
        categories: Vec::new(),
        start_urls: vec![start_url.to_string()],
        ckan_instance_ref: None,
        context_spaces: Vec::new(),
        organizations: Vec::new(),
        upload: None,
        sitemap: true,
        wordpress_types: vec![],
        include: Vec::new(),
        exclude: Vec::new(),
        max_depth: 3,
        max_pages: 100,
        pdf: PdfPolicy::default(),
        off_domain_documents: false,
        schedule: None,
        languages: Vec::new(),
        visibility: Visibility::Public,
    }
}

fn test_crawler(resolver: FixtureResolver, max_page_bytes: Option<u64>) -> Crawler {
    let policy = CrawlPolicy {
        allow_plain_http: true,
        max_page_bytes: max_page_bytes.unwrap_or(10 * 1024 * 1024),
    };
    Crawler::new(Arc::new(resolver), policy).expect("the crawler builds")
}

/// Case 1: End-to-end crawl verifying robots.txt, sitemap, links, documents, and report.
#[tokio::test]
async fn crawl_end_to_end() {
    let (admin, pool, db_name) = database("e2e").await;

    let server1 = MockServer::start().await;
    let p1 = server1.address().port();
    let server2 = MockServer::start().await;
    let p2 = server2.address().port();

    let resolver = FixtureResolver::new(&[("www.city.test", p1), ("files.other.test", p2)]);
    let crawler = test_crawler(resolver, None);

    // robots.txt disallowing /private/ and announcing the sitemap
    Mock::given(method("GET"))
        .and(path("/robots.txt"))
        .respond_with(ResponseTemplate::new(200).set_body_string(format!(
            "User-agent: *\nDisallow: /private/\nSitemap: http://www.city.test:{p1}/sitemap.xml\n"
        )))
        .mount(&server1)
        .await;

    // sitemap.xml listing /about
    Mock::given(method("GET"))
        .and(path("/sitemap.xml"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/xml")
                .set_body_string(format!(
                    "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
                     <urlset xmlns=\"http://www.sitemaps.org/schemas/sitemap/0.9\">\
                       <url><loc>http://www.city.test:{p1}/about</loc></url>\
                     </urlset>"
                )),
        )
        .mount(&server1)
        .await;

    // Root page with mixed links
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/html; charset=utf-8")
                .set_body_string(format!(
                    r##"<!DOCTYPE html>
                    <html lang="en">
                    <body>
                      <a href="/news">News</a>
                      <a href="/private/x">Private</a>
                      <a href="/report.pdf">Report</a>
                      <a href="http://files.other.test:{p2}/annex.pdf">Annex</a>
                      <a href="https://example.org/">Example</a>
                      <a href="#top">Top</a>
                    </body>
                    </html>"##
                )),
        )
        .mount(&server1)
        .await;

    // Disallowed private page: mounted but must not be requested
    Mock::given(method("GET"))
        .and(path("/private/x"))
        .respond_with(ResponseTemplate::new(200).set_body_string("private content"))
        .mount(&server1)
        .await;

    // Allowed page: /news
    Mock::given(method("GET"))
        .and(path("/news"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/html; charset=utf-8")
                .set_body_string(r#"<html lang="en"><body><h1>News</h1></body></html>"#),
        )
        .mount(&server1)
        .await;

    // Allowed sitemap page: /about, stating its title, description and publication date; its
    // change date comes from the server (AG-120).
    Mock::given(method("GET"))
        .and(path("/about"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/html; charset=utf-8")
                .insert_header("last-modified", "Wed, 30 Sep 2026 08:12:31 GMT")
                .set_body_string(
                    r#"<html lang="en"><head><title>About the city</title>
                    <meta name="description" content="Who we are">
                    <meta property="article:published_time" content="2024-03-01T10:00:00+01:00">
                    </head><body><h1>About</h1></body></html>"#,
                ),
        )
        .mount(&server1)
        .await;

    // On-domain document: /report.pdf
    let pdf_bytes = b"%PDF-1.4 sample pdf content for testing";
    Mock::given(method("GET"))
        .and(path("/report.pdf"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/pdf")
                .set_body_bytes(pdf_bytes.to_vec()),
        )
        .mount(&server1)
        .await;

    // Off-domain document on server2: must not be downloaded
    Mock::given(method("GET"))
        .and(path("/annex.pdf"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/pdf")
                .set_body_bytes(b"%PDF-1.4 annex content".to_vec()),
        )
        .mount(&server2)
        .await;

    let spec = test_spec(&format!("http://www.city.test:{p1}/"));
    let mut sink = RecordingSink::default();

    let report = crawl_site(
        &pool, &crawler, "hel", "helsinki", "city-web", &spec, &mut sink,
    )
    .await
    .expect("crawl succeeds");

    // Off-domain document on files.other.test was not downloaded
    assert!(
        server2
            .received_requests()
            .await
            .unwrap_or_default()
            .is_empty(),
        "files.other.test must receive no requests when offDomainDocuments is false"
    );

    // Private page on server1 was not fetched
    let private_requests: Vec<_> = server1
        .received_requests()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|req| req.url.path() == "/private/x")
        .collect();
    assert!(
        private_requests.is_empty(),
        "disallowed page must not be fetched"
    );

    // Assert database rows within project scope
    let mut tx = project_scope(&pool, "helsinki")
        .await
        .expect("project scope");

    let pages: Vec<(String, String, bool)> =
        sqlx::query("SELECT url, status, included FROM pages ORDER BY url")
            .fetch_all(&mut *tx)
            .await
            .expect("pages query")
            .into_iter()
            .map(|r| (r.get("url"), r.get("status"), r.get("included")))
            .collect();

    let expected_root = format!("http://www.city.test:{p1}/");
    let expected_about = format!("http://www.city.test:{p1}/about");
    let expected_news = format!("http://www.city.test:{p1}/news");
    let expected_private = format!("http://www.city.test:{p1}/private/x");

    assert!(pages.contains(&(expected_root.clone(), "fetched".to_string(), true)));
    assert!(pages.contains(&(expected_about.clone(), "fetched".to_string(), true)));
    assert!(pages.contains(&(expected_news.clone(), "fetched".to_string(), true)));
    assert!(pages.contains(&(expected_private.clone(), "skipped".to_string(), true)));

    let stated: Vec<(String, [Option<String>; 4])> =
        sqlx::query(
            "SELECT url, title, description, \
                    to_char(published AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS') AS published, \
                    to_char(modified AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS') AS modified \
             FROM pages WHERE status = 'fetched' ORDER BY url",
        )
        .fetch_all(&mut *tx)
        .await
        .expect("page metadata")
        .into_iter()
        .map(|r| {
            (
                r.get("url"),
                ["title", "description", "published", "modified"].map(|c| r.get(c)),
            )
        })
        .collect();
    assert!(
        stated.contains(&(
            expected_about.clone(),
            [
                Some("About the city".into()),
                Some("Who we are".into()),
                Some("2024-03-01T09:00:00".into()),
                Some("2026-09-30T08:12:31".into()),
            ],
        )),
        "{stated:?}"
    );
    assert!(
        stated.contains(&(expected_news.clone(), [None, None, None, None])),
        "a page that states nothing has nothing: {stated:?}"
    );

    let documents: Vec<(String, String, bool, bool)> =
        sqlx::query("SELECT url, status, included, off_domain FROM documents ORDER BY url")
            .fetch_all(&mut *tx)
            .await
            .expect("documents query")
            .into_iter()
            .map(|r| {
                (
                    r.get("url"),
                    r.get("status"),
                    r.get("included"),
                    r.get("off_domain"),
                )
            })
            .collect();

    let expected_report = format!("http://www.city.test:{p1}/report.pdf");
    let expected_annex = format!("http://files.other.test:{p2}/annex.pdf");

    assert!(documents.contains(&(expected_report.clone(), "fetched".to_string(), true, false)));
    assert!(documents.contains(&(expected_annex.clone(), "skipped".to_string(), false, true)));

    let links: Vec<(String, String)> =
        sqlx::query("SELECT to_url, kind FROM links ORDER BY to_url")
            .fetch_all(&mut *tx)
            .await
            .expect("links query")
            .into_iter()
            .map(|r| (r.get("to_url"), r.get("kind")))
            .collect();

    assert!(links.contains(&(expected_news, "page".to_string())));
    assert!(links.contains(&(expected_private, "page".to_string())));
    assert!(links.contains(&(expected_report, "document".to_string())));
    assert!(links.contains(&(expected_annex, "document".to_string())));
    assert!(links.contains(&("https://example.org/".to_string(), "external".to_string())));
    assert!(links.contains(&(expected_root, "page".to_string())));

    tx.rollback().await.expect("rollback");

    // Sink called only for included, changed items
    assert_eq!(sink.pages.len(), 3, "sink received root, about, and news");
    assert_eq!(sink.documents.len(), 1, "sink received report.pdf only");

    // Verify report counts
    assert_eq!(report.pages_fetched, 3);
    assert_eq!(report.pages_unchanged, 0);
    assert_eq!(report.documents_fetched, 1);
    assert_eq!(report.documents_unchanged, 0);
    assert_eq!(report.documents_excluded, 1);
    assert_eq!(report.disallowed, 1);
    assert_eq!(report.refused, 0);
    assert_eq!(report.too_large, 0);
    assert_eq!(report.failed, 0);
    assert_eq!(report.links, 6);

    drop_database(admin, pool, &db_name).await;
}

/// Case 2: Recrawl with unchanged content, followed by updating a document body.
#[tokio::test]
async fn crawl_recrawl_change_detection() {
    let (admin, pool, db_name) = database("recrawl").await;

    let server = MockServer::start().await;
    let port = server.address().port();
    let resolver = FixtureResolver::new(&[("www.city.test", port)]);
    let crawler = test_crawler(resolver, None);

    Mock::given(method("GET"))
        .and(path("/robots.txt"))
        .respond_with(ResponseTemplate::new(200).set_body_string("User-agent: *\nAllow: /\n"))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/html; charset=utf-8")
                .set_body_string(
                    r#"<html lang="en"><body><a href="/doc.pdf">Doc</a></body></html>"#,
                ),
        )
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/doc.pdf"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/pdf")
                .set_body_bytes(b"%PDF-1.4 version 1".to_vec()),
        )
        .mount(&server)
        .await;

    let spec = test_spec(&format!("http://www.city.test:{port}/"));

    // Initial crawl
    let mut sink1 = RecordingSink::default();
    let report1 = crawl_site(
        &pool,
        &crawler,
        "hel",
        "helsinki",
        "recrawl-src",
        &spec,
        &mut sink1,
    )
    .await
    .expect("initial crawl succeeds");
    assert_eq!(report1.pages_fetched, 1);
    assert_eq!(report1.documents_fetched, 1);
    assert_eq!(sink1.pages.len(), 1);
    assert_eq!(sink1.documents.len(), 1);

    // Second crawl: bodies are identical
    let mut sink2 = RecordingSink::default();
    let report2 = crawl_site(
        &pool,
        &crawler,
        "hel",
        "helsinki",
        "recrawl-src",
        &spec,
        &mut sink2,
    )
    .await
    .expect("second crawl succeeds");
    assert_eq!(report2.pages_fetched, 0);
    assert_eq!(report2.pages_unchanged, 1);
    assert_eq!(report2.documents_fetched, 0);
    assert_eq!(report2.documents_unchanged, 1);
    assert!(
        sink2.pages.is_empty(),
        "no pages emitted on unchanged recrawl"
    );
    assert!(
        sink2.documents.is_empty(),
        "no documents emitted on unchanged recrawl"
    );

    // Update the document body on server
    server.reset().await;
    Mock::given(method("GET"))
        .and(path("/robots.txt"))
        .respond_with(ResponseTemplate::new(200).set_body_string("User-agent: *\nAllow: /\n"))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/html; charset=utf-8")
                .set_body_string(
                    r#"<html lang="en"><body><a href="/doc.pdf">Doc</a></body></html>"#,
                ),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/doc.pdf"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/pdf")
                .set_body_bytes(b"%PDF-1.4 version 2 modified bytes".to_vec()),
        )
        .mount(&server)
        .await;

    // Third crawl: document changed
    let mut sink3 = RecordingSink::default();
    let report3 = crawl_site(
        &pool,
        &crawler,
        "hel",
        "helsinki",
        "recrawl-src",
        &spec,
        &mut sink3,
    )
    .await
    .expect("third crawl succeeds");
    assert_eq!(report3.pages_fetched, 0);
    assert_eq!(report3.pages_unchanged, 1);
    assert_eq!(report3.documents_fetched, 1);
    assert_eq!(report3.documents_unchanged, 0);
    assert!(sink3.pages.is_empty());
    assert_eq!(sink3.documents.len(), 1);
    assert_eq!(
        sink3.documents[0].3,
        b"%PDF-1.4 version 2 modified bytes".to_vec()
    );

    drop_database(admin, pool, &db_name).await;
}

/// T-5699, F-PLT1-014: one page linking 50 PDFs, under `maxPages` 5, fetches 5 of them.
#[tokio::test]
async fn crawl_reads_no_more_documents_than_max_pages() {
    let (admin, pool, db_name) = database("doccap").await;
    let server = MockServer::start().await;
    let port = server.address().port();
    let crawler = test_crawler(FixtureResolver::new(&[("www.city.test", port)]), None);
    Mock::given(method("GET"))
        .and(path("/robots.txt"))
        .respond_with(ResponseTemplate::new(200).set_body_string("User-agent: *\nAllow: /\n"))
        .mount(&server)
        .await;
    let links: String = (0..50)
        .map(|n| format!(r#"<a href="/d{n}.pdf">D{n}</a>"#))
        .collect();
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/html; charset=utf-8")
                .set_body_string(links),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(wiremock::matchers::path_regex(r"^/d[0-9]+\.pdf$"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/pdf")
                .set_body_bytes(b"%PDF-1.4 a document".to_vec()),
        )
        .mount(&server)
        .await;

    let mut spec = test_spec(&format!("http://www.city.test:{port}/"));
    spec.max_pages = 5;
    let mut sink = RecordingSink::default();
    let report = crawl_site(
        &pool,
        &crawler,
        "hel",
        "helsinki",
        "doccap-src",
        &spec,
        &mut sink,
    )
    .await
    .expect("crawl succeeds");

    let asked = server.received_requests().await.unwrap_or_default();
    let pdfs = asked
        .iter()
        .filter(|request| request.url.path().ends_with(".pdf"))
        .count();
    assert_eq!(pdfs, 5, "{report:?}");
    assert_eq!(report.removed, 0);
    drop_database(admin, pool, &db_name).await;
}

/// Case 3: Page byte cap and document byte cap enforcement.
#[tokio::test]
async fn crawl_caps_exceeded() {
    let (admin, pool, db_name) = database("caps").await;

    let server = MockServer::start().await;
    let port = server.address().port();
    let resolver = FixtureResolver::new(&[("www.city.test", port)]);

    // Crawler with a 100-byte cap for HTML pages
    let crawler = test_crawler(resolver, Some(100));

    Mock::given(method("GET"))
        .and(path("/robots.txt"))
        .respond_with(ResponseTemplate::new(200).set_body_string("User-agent: *\nAllow: /\n"))
        .mount(&server)
        .await;

    // Small root page pointing to an oversized page and an oversized PDF
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/html; charset=utf-8")
                .set_body_string(r#"<a href="/big">B</a><a href="/big.pdf">D</a>"#),
        )
        .mount(&server)
        .await;

    // Page exceeding 100 bytes
    Mock::given(method("GET"))
        .and(path("/big"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/html; charset=utf-8")
                .set_body_string("X".repeat(250)),
        )
        .mount(&server)
        .await;

    // PDF exceeding pdf.max_bytes
    Mock::given(method("GET"))
        .and(path("/big.pdf"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/pdf")
                .set_body_bytes(vec![0u8; 300]),
        )
        .mount(&server)
        .await;

    let mut spec = test_spec(&format!("http://www.city.test:{port}/"));
    spec.pdf.max_bytes = 100;

    let mut sink = RecordingSink::default();
    let report = crawl_site(
        &pool, &crawler, "hel", "helsinki", "caps-src", &spec, &mut sink,
    )
    .await
    .expect("crawl succeeds");

    assert_eq!(report.too_large, 2, "both page and doc exceeded limits");

    let mut tx = project_scope(&pool, "helsinki")
        .await
        .expect("project scope");

    let page_status: String = sqlx::query_scalar("SELECT status FROM pages WHERE url LIKE '%/big'")
        .fetch_one(&mut *tx)
        .await
        .expect("big page row");
    assert_eq!(page_status, "skipped");

    let doc_status: String =
        sqlx::query_scalar("SELECT status FROM documents WHERE url LIKE '%/big.pdf'")
            .fetch_one(&mut *tx)
            .await
            .expect("big doc row");
    assert_eq!(doc_status, "skipped");

    tx.rollback().await.expect("rollback");

    drop_database(admin, pool, &db_name).await;
}

/// A CA, or an intermediate when `issuer` is given, minted per run (no key in the repository).
fn authority(
    name: &str,
    issuer: Option<&rcgen::Issuer<'_, rcgen::KeyPair>>,
) -> (rcgen::Certificate, rcgen::KeyPair, rcgen::CertificateParams) {
    let key = rcgen::KeyPair::generate().expect("a key");
    let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).expect("params");
    params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    params.key_usages = vec![
        rcgen::KeyUsagePurpose::KeyCertSign,
        rcgen::KeyUsagePurpose::CrlSign,
    ];
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, name);
    let cert = match issuer {
        Some(issuer) => params.signed_by(&key, issuer),
        None => params.self_signed(&key),
    }
    .expect("a certificate");
    (cert, key, params)
}

/// An https server for www.city.test that sends only its leaf, as www.bbsk.sk does (T-3298).
async fn leaf_only_server(issuer: &rcgen::Issuer<'_, rcgen::KeyPair>) -> u16 {
    let key = rcgen::KeyPair::generate().expect("a server key");
    let leaf = rcgen::CertificateParams::new(vec!["www.city.test".to_owned()])
        .expect("the SAN")
        .signed_by(&key, issuer)
        .expect("a leaf");
    let tls = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(
            vec![leaf.der().clone()],
            rustls::pki_types::PrivateKeyDer::Pkcs8(key.serialize_der().into()),
        )
        .expect("the leaf matches the key");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a port");
    let port = listener.local_addr().expect("address").port();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(tls));
    tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        while let Ok((stream, _)) = listener.accept().await {
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                let Ok(mut stream) = acceptor.accept(stream).await else {
                    return;
                };
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf).await;
                let _ = stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok",
                    )
                    .await;
                let _ = stream.shutdown().await;
            });
        }
    });
    port
}

/// T-3298: a site that omits its intermediate is read once the crawler holds that intermediate;
/// without it, or with another CA's, the TLS check still refuses, and trust ends at the root.
#[tokio::test]
async fn a_missing_intermediate_the_crawler_holds_completes_the_chain() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let (root, root_key, root_params) = authority("test root", None);
    let root_issuer = rcgen::Issuer::new(root_params, root_key);
    let (intermediate, key, params) = authority("test intermediate", Some(&root_issuer));
    let issuer = rcgen::Issuer::new(params, key);
    let (stranger, _, _) = authority("another intermediate", Some(&root_issuer));
    let port = leaf_only_server(&issuer).await;

    let (other_root, _, _) = authority("another root", None);
    let fetch_with = |root: &rcgen::Certificate,
                      extra: Vec<rustls::pki_types::CertificateDer<'static>>| {
        let mut roots = rustls::RootCertStore::empty();
        roots.add(root.der().clone()).expect("the root");
        let config = fetch::tls_config(roots, extra).expect("TLS config");
        let client = fetch::client_with_tls(
            Arc::new(FixtureResolver::new(&[("www.city.test", port)])),
            config,
        )
        .expect("client");
        async move {
            client
                .get(format!("https://www.city.test:{port}/"))
                .send()
                .await
        }
    };
    assert!(
        fetch_with(&root, vec![]).await.is_err(),
        "a leaf alone builds no chain"
    );
    assert!(
        fetch_with(&root, vec![stranger.der().clone()])
            .await
            .is_err(),
        "another CA's intermediate builds no chain"
    );
    // The intermediate is offered, never trusted: under another root it reads nothing.
    assert!(
        fetch_with(&other_root, vec![intermediate.der().clone()])
            .await
            .is_err(),
        "an offered intermediate is no trust anchor"
    );
    let answer = fetch_with(&root, vec![intermediate.der().clone()])
        .await
        .expect("the chain completes");
    assert_eq!(answer.status(), 200);
}

#[test]
fn the_shipped_intermediates_are_the_pinned_ones() {
    use sha2::{Digest, Sha256};
    let pinned: Vec<(&str, String)> = fetch::INTERMEDIATES
        .iter()
        .map(|(name, der)| (*name, hex::encode(Sha256::digest(der))))
        .collect();
    assert_eq!(
        pinned,
        vec![
            (
                "GeoTrust TLS RSA CA G1",
                "c06e307f7cfc1d32fa72a4c033c87b90019af216f0775d64978a2eca6c8a230e".to_owned()
            ),
            (
                "Thawte TLS RSA CA G1",
                "4bcc5e234fe81ede4eaf883aa19c31335b0b26e85e066b9945e4cb6153eb20c2".to_owned()
            ),
        ]
    );
}

/// T-3293: a start address that is a document, by its name or by the content type it answers
/// with, is read as one document of the site under the `pdf` policy, never as a page of its bytes.
#[tokio::test]
async fn a_start_address_that_is_a_pdf_is_read_as_a_document() {
    let (admin, pool, db_name) = database("start_pdf").await;
    let server = MockServer::start().await;
    let port = server.address().port();
    let crawler = test_crawler(FixtureResolver::new(&[("www.city.test", port)]), None);
    Mock::given(method("GET"))
        .and(path("/robots.txt"))
        .respond_with(ResponseTemplate::new(200).set_body_string("User-agent: *\nAllow: /\n"))
        .mount(&server)
        .await;
    let pdf = b"%PDF-1.4 the waste calendar".to_vec();
    for at in ["/calendar.pdf", "/download"] {
        Mock::given(method("GET"))
            .and(path(at))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/pdf")
                    .set_body_bytes(pdf.clone()),
            )
            .mount(&server)
            .await;
    }

    let mut spec = test_spec(&format!("http://www.city.test:{port}/calendar.pdf"));
    spec.start_urls
        .push(format!("http://www.city.test:{port}/download"));
    spec.sitemap = false;
    let mut sink = RecordingSink::default();
    let report = crawl_site(
        &pool, &crawler, "hel", "helsinki", "pdf-src", &spec, &mut sink,
    )
    .await
    .expect("crawl succeeds");

    assert!(
        sink.pages.is_empty(),
        "no page of a PDF's bytes: {:?}",
        sink.pages
    );
    let mut read: Vec<_> = sink
        .documents
        .iter()
        .map(|(_, url, mime, bytes)| (url.clone(), mime.clone(), bytes.clone()))
        .collect();
    read.sort();
    assert_eq!(
        read,
        vec![
            (
                format!("http://www.city.test:{port}/calendar.pdf"),
                Some("application/pdf".into()),
                pdf.clone()
            ),
            (
                format!("http://www.city.test:{port}/download"),
                Some("application/pdf".into()),
                pdf.clone()
            ),
        ]
    );
    assert_eq!((report.documents_fetched, report.pages_fetched), (2, 0));
    let mut tx = project_scope(&pool, "helsinki").await.expect("scope");
    let pages: i64 = sqlx::query_scalar("SELECT count(*) FROM pages")
        .fetch_one(&mut *tx)
        .await
        .expect("pages");
    let fetched: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM documents WHERE status = 'fetched' AND page_id IS NULL",
    )
    .fetch_one(&mut *tx)
    .await
    .expect("documents");
    assert_eq!((pages, fetched), (0, 2));
    tx.rollback().await.expect("rollback");

    // `pdf.policy: exclude` reads nothing from either, by name or by content type.
    let mut sink = RecordingSink::default();
    spec.pdf.policy = PdfPolicyKind::Exclude;
    let (admin2, pool2, db2) = database("start_pdf_excluded").await;
    let report = crawl_site(
        &pool2, &crawler, "hel", "helsinki", "pdf-src", &spec, &mut sink,
    )
    .await
    .expect("crawl succeeds");
    assert!(sink.pages.is_empty() && sink.documents.is_empty());
    assert_eq!(report.documents_excluded, 2);
    drop_database(admin2, pool2, &db2).await;
    drop_database(admin, pool, &db_name).await;
}

/// Case 4: SSRF guard refusing loopback redirects, link literals, and PublicOnly egress check.
/// T-3059: a redirect to the same host on another port is another service, and is not followed.
#[tokio::test]
async fn a_redirect_to_another_port_of_the_same_host_is_not_followed() {
    let (admin, pool, db_name) = database("redirectport").await;
    let site = MockServer::start().await;
    let other = MockServer::start().await;
    let port = site.address().port();
    let crawler = test_crawler(FixtureResolver::new(&[("www.city.test", port)]), None);
    Mock::given(method("GET"))
        .and(path("/robots.txt"))
        .respond_with(ResponseTemplate::new(200).set_body_string("User-agent: *\nAllow: /\n"))
        .mount(&site)
        .await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/html; charset=utf-8")
                .set_body_string(r#"<a href="/moved">Moved</a>"#),
        )
        .mount(&site)
        .await;
    Mock::given(method("GET"))
        .and(path("/moved"))
        .respond_with(ResponseTemplate::new(302).insert_header(
            "location",
            format!("http://www.city.test:{}/elsewhere", other.address().port()),
        ))
        .mount(&site)
        .await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string("another service"))
        .mount(&other)
        .await;

    let spec = test_spec(&format!("http://www.city.test:{port}/"));
    let mut sink = RecordingSink::default();
    crawl_site(
        &pool, &crawler, "hel", "helsinki", "port-src", &spec, &mut sink,
    )
    .await
    .expect("crawl finishes");
    assert!(
        other
            .received_requests()
            .await
            .unwrap_or_default()
            .is_empty(),
        "the other port was reached"
    );
    drop_database(admin, pool, &db_name).await;
}

#[tokio::test]
async fn crawl_ssrf_protection() {
    let (admin, pool, db_name) = database("ssrf").await;

    let server = MockServer::start().await;
    let port = server.address().port();
    let resolver = FixtureResolver::new(&[("www.city.test", port)]);
    let crawler = test_crawler(resolver, None);

    Mock::given(method("GET"))
        .and(path("/robots.txt"))
        .respond_with(ResponseTemplate::new(200).set_body_string("User-agent: *\nAllow: /\n"))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/html; charset=utf-8")
                .set_body_string(r#"<a href="/redirect">R</a><a href="http://[::1]/x">Local</a>"#),
        )
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/redirect"))
        .respond_with(
            ResponseTemplate::new(302)
                .insert_header("location", format!("http://127.0.0.1:{port}/secret")),
        )
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/secret"))
        .respond_with(ResponseTemplate::new(200).set_body_string("secret data"))
        .mount(&server)
        .await;

    let spec = test_spec(&format!("http://www.city.test:{port}/"));
    let mut sink = RecordingSink::default();

    let report = crawl_site(
        &pool, &crawler, "hel", "helsinki", "ssrf-src", &spec, &mut sink,
    )
    .await
    .expect("crawl finishes");

    assert!(report.refused >= 1, "SSRF attempts were refused");

    let secret_requests: Vec<_> = server
        .received_requests()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|req| req.url.path() == "/secret")
        .collect();
    assert!(
        secret_requests.is_empty(),
        "private address redirect must never be fetched"
    );

    // Egress resolver check with PublicOnly against localhost
    let egress_client = fetch::client(Arc::new(PublicOnly)).expect("the client builds");
    let err = egress_client
        .get("https://localhost/")
        .send()
        .await
        .expect_err("fetching localhost must fail with PublicOnly resolver");
    // The refusal sits in the error's source chain, under reqwest's own connect error.
    let mut cause: Option<&(dyn std::error::Error + 'static)> = Some(&err);
    let mut refusal = None;
    while let Some(current) = cause {
        if let Some(found) = refused(current) {
            refusal = Some(found.host.clone());
        }
        cause = current.source();
    }
    assert_eq!(refusal.as_deref(), Some("localhost"), "{err:?}");

    drop_database(admin, pool, &db_name).await;
}

/// Case 5: Path globs and include/exclude pattern matching.
#[test]
fn patterns_glob_matching() {
    assert!(patterns::glob_match("/a/**", "/a/b"));
    assert!(patterns::glob_match("/a/**", "/a/b/c"));
    assert!(patterns::glob_match("/a/**", "/a/"));
    assert!(!patterns::glob_match("/a/**", "/b/c"));
    assert!(!patterns::glob_match("/a/**", "/a"));

    assert!(patterns::glob_match("/a/*/c", "/a/b/c"));
    assert!(patterns::glob_match("/a/*/c", "/a/x/c"));
    assert!(!patterns::glob_match("/a/*/c", "/a/b/d/c"));
    assert!(!patterns::glob_match("/a/*/c", "/a/c"));

    let inc = vec!["/a/**".to_string()];
    let exc = vec!["/a/private/**".to_string()];
    assert!(patterns::is_included("/a/public/page", &inc, &exc));
    assert!(!patterns::is_included("/a/private/page", &inc, &exc));
    assert!(!patterns::is_included("/other", &inc, &exc));

    let inc_empty: Vec<String> = Vec::new();
    let exc_admin = vec!["/admin/**".to_string()];
    assert!(patterns::is_included("/index.html", &inc_empty, &exc_admin));
    assert!(patterns::is_included("/about", &inc_empty, &exc_admin));
    assert!(!patterns::is_included(
        "/admin/dashboard",
        &inc_empty,
        &exc_admin
    ));
}

/// Case 6: Concurrent claim locking and 5-failure threshold transition to failed.
#[tokio::test]
async fn queue_concurrent_claim_and_retry_limits() {
    let (admin, pool, db_name) = database("queue").await;

    // Concurrent claim on a single job
    let job_id = queue::enqueue(&pool, "queue-proj", "source-1")
        .await
        .expect("enqueue job");

    let claim1 = queue::claim(&pool, "worker-1");
    let claim2 = queue::claim(&pool, "worker-2");
    let (res1, res2) = tokio::join!(claim1, claim2);

    let j1 = res1.expect("claim 1 succeeds");
    let j2 = res2.expect("claim 2 succeeds");

    assert!(
        j1.is_some() ^ j2.is_some(),
        "exactly one worker claims the queued job"
    );
    let claimed = j1.or(j2).expect("a job was claimed");
    assert_eq!(claimed.id, job_id);

    queue::finish(&pool, job_id).await.expect("finish job");
    let state: String = sqlx::query_scalar("SELECT state FROM crawl_jobs WHERE id = $1")
        .bind(job_id)
        .fetch_one(&pool)
        .await
        .expect("state");
    assert_eq!(state, "done");

    // Failure retry progression up to 5 attempts
    let fail_job_id = queue::enqueue(&pool, "queue-proj", "source-fail")
        .await
        .expect("enqueue fail job");

    for attempt in 1..=5 {
        queue::fail(&pool, fail_job_id, &format!("attempt {attempt} error"))
            .await
            .expect("fail succeeds");
    }

    let row = sqlx::query("SELECT state, attempts, error FROM crawl_jobs WHERE id = $1")
        .bind(fail_job_id)
        .fetch_one(&pool)
        .await
        .expect("job row");
    let final_state: String = row.get("state");
    let attempts: i32 = row.get("attempts");
    let err_msg: String = row.get("error");

    assert_eq!(final_state, "failed");
    assert_eq!(attempts, 5);
    assert_eq!(err_msg, "attempt 5 error");

    drop_database(admin, pool, &db_name).await;
}

/// T-4278: a job whose worker died mid-crawl (a pod replaced by an apply) stayed `running` for
/// good, so it was never claimed again and `pending` kept every later job of the source out:
/// web-bbsk stopped at 84 of bbsk.sk's pages. A running job past its lease is claimed again, as a
/// further attempt; one within its lease is not.
#[tokio::test]
async fn a_job_left_running_by_a_dead_worker_is_claimed_again_after_its_lease() {
    let (admin, pool, db_name) = database("stale").await;
    let job_id = queue::enqueue(&pool, "stale-proj", "web")
        .await
        .expect("enqueue");
    let first = queue::claim(&pool, "jc-assistant-old")
        .await
        .expect("claim");
    assert_eq!(first.map(|job| job.id), Some(job_id));

    // Still within its lease: the worker may be crawling, so nobody else takes it.
    assert_eq!(
        queue::claim(&pool, "jc-assistant-new")
            .await
            .expect("claim"),
        None
    );
    assert!(queue::pending(&pool, "stale-proj", "web")
        .await
        .expect("pending"));

    sqlx::query("UPDATE crawl_jobs SET claimed_at = now() - interval '7 hours' WHERE id = $1")
        .bind(job_id)
        .execute(&pool)
        .await
        .expect("age the claim");
    let again = queue::claim(&pool, "jc-assistant-new")
        .await
        .expect("claim")
        .expect("the stale job is claimed again");
    assert_eq!((again.id, again.attempts), (job_id, 1));
    let owner: String = sqlx::query_scalar("SELECT claimed_by FROM crawl_jobs WHERE id = $1")
        .bind(job_id)
        .fetch_one(&pool)
        .await
        .expect("owner");
    assert_eq!(owner, "jc-assistant-new");

    // A job that took its worker down five times is failed, not claimed a sixth time.
    sqlx::query(
        "UPDATE crawl_jobs SET attempts = 5, claimed_at = now() - interval '7 hours' WHERE id = $1",
    )
    .bind(job_id)
    .execute(&pool)
    .await
    .expect("five attempts");
    assert_eq!(
        queue::claim(&pool, "jc-assistant-new")
            .await
            .expect("claim"),
        None
    );
    let state: String = sqlx::query_scalar("SELECT state FROM crawl_jobs WHERE id = $1")
        .bind(job_id)
        .fetch_one(&pool)
        .await
        .expect("state");
    assert_eq!(state, "failed");
    // And no longer keeps the source's next job out.
    assert!(!queue::pending(&pool, "stale-proj", "web")
        .await
        .expect("pending"));

    drop_database(admin, pool, &db_name).await;
}

/// Case 7: robots.txt HTTP status semantics (404 allows, 500 disallows).
#[tokio::test]
async fn robots_http_status_semantics() {
    let server = MockServer::start().await;
    let port = server.address().port();
    let resolver = FixtureResolver::new(&[("www.city.test", port)]);
    let client = fetch::client(Arc::new(resolver)).expect("the client builds");
    let policy = CrawlPolicy {
        allow_plain_http: true,
        max_page_bytes: 10 * 1024 * 1024,
    };

    // 404 response -> AllowAll
    Mock::given(method("GET"))
        .and(path("/robots.txt"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    let (robots_404, _) = robots::fetch_robots(
        &client,
        &policy,
        "www.city.test",
        &format!("http://www.city.test:{port}/robots.txt"),
    )
    .await;
    assert!(robots_404.is_allowed(&format!("http://www.city.test:{port}/anything")));

    // 500 response -> DisallowAll
    server.reset().await;
    Mock::given(method("GET"))
        .and(path("/robots.txt"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;

    let (robots_500, _) = robots::fetch_robots(
        &client,
        &policy,
        "www.city.test",
        &format!("http://www.city.test:{port}/robots.txt"),
    )
    .await;
    assert!(!robots_500.is_allowed(&format!("http://www.city.test:{port}/anything")));
}

/// Refusal of non-website KnowledgeSourceSpec.
#[tokio::test]
async fn spec_validation_refuses_ckan_source() {
    let (admin, pool, db_name) = database("ckan_refusal").await;

    let crawler = test_crawler(FixtureResolver::new(&[]), None);
    let mut spec = test_spec("http://www.city.test/");
    spec.source = SourceType::Ckan;
    spec.ckan_instance_ref = Some("catalogue".to_string());
    spec.start_urls = Vec::new();

    let mut sink = RecordingSink::default();
    let outcome = crawl_site(
        &pool, &crawler, "hel", "helsinki", "ckan-src", &spec, &mut sink,
    )
    .await;

    assert!(matches!(outcome, Err(assistant::Error::Crawl(_))));

    drop_database(admin, pool, &db_name).await;
}

/// T-4154: a WordPress source's pager reads each type newest first until a short page, skips a
/// type the site does not have, keeps the site's own host only, and stops at `maxPages`.
#[tokio::test]
async fn wordpress_types_are_paged_newest_first_on_the_site_host_only() {
    let server = MockServer::start().await;
    let port = server.address().port();
    let resolver = FixtureResolver::new(&[("www.city.test", port)]);
    let client = fetch::client(Arc::new(resolver)).expect("the client builds");
    let policy = CrawlPolicy {
        allow_plain_http: true,
        max_page_bytes: 1024 * 1024,
    };
    let site = format!("http://www.city.test:{port}");
    let links = |from: usize, to: usize| {
        serde_json::Value::Array(
            (from..to)
                .map(|i| serde_json::json!({ "link": format!("{site}/stranka-{i}/") }))
                .collect(),
        )
    };
    let mut first = links(0, wordpress::PER_PAGE);
    // Another host and a repeat of an earlier item never become crawl addresses.
    first.as_array_mut().unwrap()[1] = serde_json::json!({ "link": "https://evil.test/x/" });
    first.as_array_mut().unwrap()[2] = serde_json::json!({ "link": format!("{site}/stranka-0/") });
    Mock::given(method("GET"))
        .and(path("/wp-json/wp/v2/pages"))
        .and(query_param("page", "1"))
        .and(query_param("orderby", "modified"))
        .and(query_param("order", "desc"))
        .and(query_param("_fields", "link"))
        .respond_with(ResponseTemplate::new(200).set_body_json(first))
        // Once for the full read, once for the capped one, which stops inside it.
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/wp-json/wp/v2/pages"))
        .and(query_param("page", "2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(links(500, 503)))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/wp-json/wp/v2/missing"))
        .respond_with(ResponseTemplate::new(404).set_body_json(
            serde_json::json!({ "code": "rest_no_route", "data": { "status": 404 } }),
        ))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/wp-json/wp/v2/newses"))
        .respond_with(ResponseTemplate::new(200).set_body_json(links(900, 902)))
        .mount(&server)
        .await;

    let start = url::Url::parse(&format!("{site}/")).unwrap();
    let types = ["pages", "missing", "newses"].map(String::from);
    let found =
        wordpress::fetch_wordpress_urls(&client, &policy, "www.city.test", &start, &types, 1000)
            .await;
    let paths: Vec<&str> = found.iter().map(|u| u.path()).collect();
    assert_eq!(found.len(), wordpress::PER_PAGE - 2 + 3 + 2);
    assert_eq!(paths[0], "/stranka-0/");
    assert!(!paths.contains(&"/x/"), "a link on another host was kept");
    assert_eq!(
        &paths[paths.len() - 5..],
        [
            "/stranka-500/",
            "/stranka-501/",
            "/stranka-502/",
            "/stranka-900/",
            "/stranka-901/"
        ]
    );

    let capped =
        wordpress::fetch_wordpress_urls(&client, &policy, "www.city.test", &start, &types, 10)
            .await;
    assert_eq!(capped.len(), 10);
    let none =
        wordpress::fetch_wordpress_urls(&client, &policy, "www.city.test", &start, &[], 10).await;
    assert!(none.is_empty());
}

/// T-4749: www.zilina.sk and www.praha.eu answer every request with a redirect to the same name
/// without `www.`, and the crawl skipped its start page as "leaving the site" and read nothing.
/// The twin host is adopted; a redirect to any other host is not.
#[tokio::test]
async fn a_start_host_that_redirects_to_its_twin_is_crawled_under_the_twin() {
    use wiremock::matchers::header;
    let (admin, pool, db_name) = database("twinhost").await;
    let server = MockServer::start().await;
    let port = server.address().port();
    let crawler = test_crawler(
        FixtureResolver::new(&[
            ("www.city.test", port),
            ("city.test", port),
            ("other.test", port),
        ]),
        None,
    );
    let www = format!("www.city.test:{port}");
    let bare = format!("city.test:{port}");
    Mock::given(method("GET"))
        .and(header("host", www.as_str()))
        .respond_with(
            ResponseTemplate::new(301).insert_header("location", format!("http://{bare}/web/")),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(header("host", bare.as_str()))
        .and(path("/robots.txt"))
        .respond_with(ResponseTemplate::new(200).set_body_string("User-agent: *\nAllow: /\n"))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(header("host", bare.as_str()))
        .and(path("/web/"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/html; charset=utf-8")
                .set_body_string(r#"<html lang="sk"><a href="/web/urad">Úrad</a></html>"#),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(header("host", bare.as_str()))
        .and(path("/web/urad"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/html; charset=utf-8")
                .set_body_string("<p>Úradné hodiny</p>"),
        )
        .mount(&server)
        .await;

    let mut spec = test_spec(&format!("http://{www}/"));
    spec.sitemap = false;
    let mut sink = RecordingSink::default();
    let report = crawl_site(&pool, &crawler, "za", "zilina", "web", &spec, &mut sink)
        .await
        .expect("crawl finishes");
    let read: Vec<&str> = sink.pages.iter().map(|p| p.1.as_str()).collect();
    assert_eq!(
        read,
        [
            format!("http://{bare}/web/").as_str(),
            format!("http://{bare}/web/urad").as_str()
        ]
    );
    assert_eq!(report.pages_fetched, 2);

    // A start address that redirects to an unrelated host is not adopted: nothing is read.
    let elsewhere = MockServer::start().await;
    let eport = elsewhere.address().port();
    let crawler = test_crawler(
        FixtureResolver::new(&[("www.town.test", eport), ("other.test", eport)]),
        None,
    );
    Mock::given(method("GET"))
        .and(header("host", format!("www.town.test:{eport}").as_str()))
        .respond_with(
            ResponseTemplate::new(301)
                .insert_header("location", format!("http://other.test:{eport}/")),
        )
        .mount(&elsewhere)
        .await;
    Mock::given(method("GET"))
        .and(header("host", format!("other.test:{eport}").as_str()))
        .respond_with(ResponseTemplate::new(200).set_body_string("<p>another site</p>"))
        .mount(&elsewhere)
        .await;
    let mut spec = test_spec(&format!("http://www.town.test:{eport}/"));
    spec.sitemap = false;
    let mut sink = RecordingSink::default();
    crawl_site(&pool, &crawler, "tw", "town", "web", &spec, &mut sink)
        .await
        .expect("crawl finishes");
    assert!(sink.pages.is_empty(), "{:?}", sink.pages);
    drop_database(admin, pool, &db_name).await;
}

/// T-4749: Banská Bystrica keeps its documents on its Azure storage. With `offDomainDocuments`
/// they are read, each under its own host's robots.txt, never the linking site's.
#[tokio::test]
async fn a_document_on_another_host_answers_to_that_hosts_robots() {
    let (admin, pool, db_name) = database("foreignrobots").await;
    let site = MockServer::start().await;
    let files = MockServer::start().await;
    let (sport, fport) = (site.address().port(), files.address().port());
    let crawler = test_crawler(
        FixtureResolver::new(&[("www.city.test", sport), ("files.test", fport)]),
        None,
    );
    Mock::given(method("GET"))
        .and(path("/robots.txt"))
        .respond_with(ResponseTemplate::new(200).set_body_string("User-agent: *\nAllow: /\n"))
        .mount(&site)
        .await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/html; charset=utf-8")
                .set_body_string(format!(
                    r#"<a href="http://files.test:{fport}/open/vzn.pdf">VZN</a>
                       <a href="http://files.test:{fport}/private/plat.pdf">Plat</a>"#
                )),
        )
        .mount(&site)
        .await;
    Mock::given(method("GET"))
        .and(path("/robots.txt"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string("User-agent: *\nDisallow: /private/\n"),
        )
        .expect(1)
        .mount(&files)
        .await;
    Mock::given(method("GET"))
        .and(path("/open/vzn.pdf"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/pdf")
                .set_body_bytes(b"%PDF-1.4 vzn".to_vec()),
        )
        .mount(&files)
        .await;
    Mock::given(method("GET"))
        .and(path("/private/plat.pdf"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"%PDF-1.4 plat".to_vec()))
        .expect(0)
        .mount(&files)
        .await;

    let mut spec = test_spec(&format!("http://www.city.test:{sport}/"));
    spec.sitemap = false;
    spec.off_domain_documents = true;
    let mut sink = RecordingSink::default();
    let report = crawl_site(
        &pool,
        &crawler,
        "bb",
        "banskabystrica",
        "web",
        &spec,
        &mut sink,
    )
    .await
    .expect("crawl finishes");
    let read: Vec<&str> = sink.documents.iter().map(|d| d.1.as_str()).collect();
    assert_eq!(
        read,
        [format!("http://files.test:{fport}/open/vzn.pdf").as_str()]
    );
    assert_eq!(
        report.disallowed, 1,
        "the private file is refused by its host's robots.txt"
    );
    drop_database(admin, pool, &db_name).await;
}

/// One run of the removal fixture: the start page links `links`, each path answers `status`.
async fn mount_site(server: &MockServer, robots: u16, links: &[&str], answers: &[(&str, u16)]) {
    server.reset().await;
    Mock::given(method("GET"))
        .and(path("/robots.txt"))
        .respond_with(ResponseTemplate::new(robots).set_body_string("User-agent: *\nAllow: /\n"))
        .mount(server)
        .await;
    let body: String = links
        .iter()
        .map(|l| format!(r#"<a href="{l}">{l}</a>"#))
        .collect();
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/html; charset=utf-8")
                .set_body_string(format!("<html><body>{body}</body></html>")),
        )
        .mount(server)
        .await;
    for (p, status) in answers {
        let kind = if p.ends_with(".pdf") {
            "application/pdf"
        } else {
            "text/html; charset=utf-8"
        };
        Mock::given(method("GET"))
            .and(path(*p))
            .respond_with(
                ResponseTemplate::new(*status)
                    .insert_header("content-type", kind)
                    .set_body_string(format!("%PDF-1.4 <p>{p}</p>")),
            )
            .mount(server)
            .await;
    }
}

/// The rows of the site as (path, failed_runs), pages then documents.
async fn rows(pool: &PgPool) -> Vec<(String, i16)> {
    let mut tx = project_scope(pool, "bbsk").await.expect("scope");
    let rows: Vec<(String, i16)> = sqlx::query_as(
        "SELECT regexp_replace(url, '^http://[^/]+', ''), failed_runs FROM pages \
         UNION ALL SELECT regexp_replace(url, '^http://[^/]+', ''), failed_runs FROM documents \
         ORDER BY 1",
    )
    .fetch_all(&mut *tx)
    .await
    .expect("rows");
    tx.commit().await.expect("commit");
    rows
}

/// T-4749: a page or document the site says is gone (404/410) leaves the index with its chunks
/// in the same run; one that fails (5xx) or that a complete crawl no longer reaches is kept
/// until its third run in a row; a run that could not read robots.txt removes nothing.
#[tokio::test]
async fn a_page_removed_from_the_site_leaves_the_index_and_a_flaky_one_stays_two_runs() {
    let (admin, pool, db_name) = database("removal").await;
    let server = MockServer::start().await;
    let port = server.address().port();
    let crawler = test_crawler(FixtureResolver::new(&[("www.city.test", port)]), None);
    let mut spec = test_spec(&format!("http://www.city.test:{port}/"));
    spec.sitemap = false;
    let crawl = || async {
        let mut sink = RecordingSink::default();
        crawl_site(&pool, &crawler, "bb", "bbsk", "web", &spec, &mut sink).await
    };

    let all = ["/a", "/b", "/c", "/doc.pdf", "/gone.pdf"];
    mount_site(&server, 200, &all, &all.map(|p| (p, 200))).await;
    let first = crawl().await.expect("crawl 1");
    assert_eq!((first.pages_fetched, first.documents_fetched), (4, 2));
    // One chunk of /a, as the indexer would have stored it.
    let mut tx = project_scope(&pool, "bbsk").await.expect("scope");
    sqlx::query(
        "INSERT INTO chunks (site_id, project, page_id, ordinal, url, text, visibility) \
         SELECT site_id, project, id, 0, url, 'A', 'public' FROM pages WHERE url LIKE '%/a'",
    )
    .execute(&mut *tx)
    .await
    .expect("chunk");
    tx.commit().await.expect("commit");

    // /a and /gone.pdf are gone, /b fails, /c is no longer linked but still answers.
    let later = ["/a", "/b", "/doc.pdf", "/gone.pdf"];
    let answers = [
        ("/a", 404),
        ("/b", 503),
        ("/c", 200),
        ("/doc.pdf", 200),
        ("/gone.pdf", 410),
    ];
    mount_site(&server, 200, &later, &answers).await;
    let second = crawl().await.expect("crawl 2");
    assert_eq!(second.removed, 2, "{second:?}");
    assert_eq!(
        rows(&pool).await,
        [
            ("/".into(), 0),
            ("/b".into(), 1),
            ("/c".into(), 1),
            ("/doc.pdf".into(), 0)
        ]
    );
    let mut tx = project_scope(&pool, "bbsk").await.expect("scope");
    let chunks: i64 = sqlx::query_scalar("SELECT count(*) FROM chunks")
        .fetch_one(&mut *tx)
        .await
        .expect("count");
    tx.commit().await.expect("commit");
    assert_eq!(chunks, 0, "the chunk of /a went with its page");

    let third = crawl().await.expect("crawl 3");
    assert_eq!(third.removed, 0);

    // A day robots.txt answers 503 the crawl reads nothing and says nothing about what is gone.
    mount_site(&server, 503, &later, &answers).await;
    let blind = crawl().await.expect("blind crawl");
    assert_eq!(blind.removed, 0);
    assert_eq!(
        rows(&pool).await,
        [
            ("/".into(), 0),
            ("/b".into(), 2),
            ("/c".into(), 2),
            ("/doc.pdf".into(), 0)
        ]
    );

    mount_site(&server, 200, &later, &answers).await;
    let fourth = crawl().await.expect("crawl 4");
    assert_eq!(fourth.removed, 2, "{fourth:?}");
    assert_eq!(rows(&pool).await, [("/".into(), 0), ("/doc.pdf".into(), 0)]);
    drop_database(admin, pool, &db_name).await;
}

/// T-4749: a page that states no date of change takes its sitemap's `<lastmod>`; one that states
/// its own keeps it; both say when they were read.
#[tokio::test]
async fn a_page_without_its_own_date_takes_the_sitemaps_lastmod() {
    let (admin, pool, db_name) = database("lastmod").await;
    let server = MockServer::start().await;
    let port = server.address().port();
    let crawler = test_crawler(FixtureResolver::new(&[("www.city.test", port)]), None);
    let base = format!("http://www.city.test:{port}");
    Mock::given(method("GET"))
        .and(path("/robots.txt"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/sitemap.xml"))
        .respond_with(ResponseTemplate::new(200).set_body_string(format!(
            r#"<urlset><url><loc>{base}/plain</loc><lastmod>2026-03-12</lastmod></url>
               <url><loc>{base}/stated</loc><lastmod>2026-03-12</lastmod></url></urlset>"#
        )))
        .mount(&server)
        .await;
    let html = |head: &str| {
        ResponseTemplate::new(200)
            .insert_header("content-type", "text/html; charset=utf-8")
            .set_body_string(format!("<html><head>{head}</head><body>text</body></html>"))
    };
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(html(""))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/plain"))
        .respond_with(html(""))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/stated"))
        .respond_with(html(
            r#"<meta property="article:modified_time" content="2026-09-22T13:58:36+0300">"#,
        ))
        .mount(&server)
        .await;

    let spec = test_spec(&format!("{base}/"));
    crawl_site(
        &pool,
        &crawler,
        "za",
        "zilina",
        "web",
        &spec,
        &mut RecordingSink::default(),
    )
    .await
    .expect("crawl finishes");
    let mut tx = project_scope(&pool, "zilina").await.expect("scope");
    let rows: Vec<(String, Option<String>, bool)> = sqlx::query_as(
        "SELECT regexp_replace(url, '^http://[^/]+', ''), \
                to_char(modified AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'), \
                fetched_at IS NOT NULL \
         FROM pages ORDER BY url",
    )
    .fetch_all(&mut *tx)
    .await
    .expect("pages");
    tx.commit().await.expect("commit");
    assert_eq!(
        rows,
        [
            ("/".into(), None, true),
            ("/plain".into(), Some("2026-03-12T00:00:00Z".into()), true),
            ("/stated".into(), Some("2026-09-22T10:58:36Z".into()), true),
        ]
    );
    drop_database(admin, pool, &db_name).await;
}

/// T-4749: the daily sitemap check reads only what its sitemap lists as new, or as changed
/// after the crawl last read it, follows none of their links, and marks the job's kind.
#[tokio::test]
async fn the_sitemap_check_reads_only_what_changed_since_it_was_read() {
    let (admin, pool, db_name) = database("sitemapcheck").await;
    let server = MockServer::start().await;
    let port = server.address().port();
    let crawler = test_crawler(FixtureResolver::new(&[("www.city.test", port)]), None);
    let base = format!("http://www.city.test:{port}");
    let mount = |sitemap: String| {
        let server = &server;
        async move {
            server.reset().await;
            Mock::given(method("GET"))
                .and(path("/robots.txt"))
                .respond_with(ResponseTemplate::new(404))
                .mount(server)
                .await;
            Mock::given(method("GET"))
                .and(path("/sitemap.xml"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_string(format!("<urlset>{sitemap}</urlset>")),
                )
                .mount(server)
                .await;
            for p in ["/", "/a", "/b", "/linked", "/new", "/deeper"] {
                let links = match p {
                    "/" => r#"<a href="/a">a</a><a href="/linked">l</a>"#,
                    "/new" => r#"<a href="/deeper">d</a>"#,
                    _ => "",
                };
                Mock::given(method("GET"))
                    .and(path(p))
                    .respond_with(
                        ResponseTemplate::new(200)
                            .insert_header("content-type", "text/html; charset=utf-8")
                            .set_body_string(format!("<html><body>{p} {links}</body></html>")),
                    )
                    .mount(server)
                    .await;
            }
        }
    };
    let url = |p: &str, lastmod: &str| {
        let lastmod = if lastmod.is_empty() {
            String::new()
        } else {
            format!("<lastmod>{lastmod}</lastmod>")
        };
        format!("<url><loc>{base}{p}</loc>{lastmod}</url>")
    };
    mount(url("/a", "2020-01-01") + &url("/b", "2020-01-01")).await;
    let spec = test_spec(&format!("{base}/"));
    let full = crawl_site(
        &pool,
        &crawler,
        "za",
        "zilina",
        "web",
        &spec,
        &mut RecordingSink::default(),
    )
    .await
    .expect("full crawl");
    assert_eq!(full.pages_fetched, 4, "{full:?}");

    // /a is as it was, /b changed after it was read, /new is new and links /deeper.
    mount(url("/a", "2020-01-01") + &url("/b", "2999-01-01") + &url("/new", "")).await;
    let check = assistant::crawl::check_sitemap(
        &pool,
        &crawler,
        "za",
        "zilina",
        "web",
        &spec,
        &mut RecordingSink::default(),
    )
    .await
    .expect("sitemap check");
    let mut read: Vec<String> = server
        .received_requests()
        .await
        .expect("recorded")
        .iter()
        .map(|r| r.url.path().to_owned())
        .filter(|p| p != "/robots.txt" && p != "/sitemap.xml")
        .collect();
    read.sort();
    // The start address is asked once, to learn the host the site answers under.
    assert_eq!(read, ["/", "/b", "/new"]);
    assert_eq!(check.pages_fetched + check.pages_unchanged, 2, "{check:?}");
    assert_eq!(check.removed, 0, "a check ages nothing");

    let job = queue::enqueue_check(&pool, "zilina", "web")
        .await
        .expect("queued");
    let claimed = queue::claim(&pool, "w")
        .await
        .expect("claim")
        .expect("a job");
    assert_eq!((claimed.id, claimed.sitemap_only), (job, true));
    queue::enqueue(&pool, "zilina", "web")
        .await
        .expect("queued");
    let full_job = queue::claim(&pool, "w")
        .await
        .expect("claim")
        .expect("a job");
    assert!(!full_job.sitemap_only);
    drop_database(admin, pool, &db_name).await;
}
