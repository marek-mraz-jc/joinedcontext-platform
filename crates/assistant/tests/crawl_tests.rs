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
use assistant::crawl::{crawl_site, CrawlPolicy, Crawler, Sink};
use assistant::{project_scope, MIGRATOR};
use jc_core::kinds::assistant::{KnowledgeSourceSpec, PdfPolicy, SourceType, Visibility};
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{ConnectOptions, PgPool, Row};
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
        start_urls: vec![start_url.to_string()],
        ckan_instance_ref: None,
        context_spaces: Vec::new(),
        sitemap: true,
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

    // Allowed sitemap page: /about
    Mock::given(method("GET"))
        .and(path("/about"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/html; charset=utf-8")
                .set_body_string(r#"<html lang="en"><body><h1>About</h1></body></html>"#),
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

/// Case 4: SSRF guard refusing loopback redirects, link literals, and PublicOnly egress check.
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
