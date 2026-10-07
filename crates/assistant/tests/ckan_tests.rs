//! A `ckan` knowledge source against a fixture catalogue (T-3054): the first run reads every
//! public dataset, the next only what changed, a dataset the catalogue drops is removed, and a
//! listing that fails removes nothing. Needs the test database (see `common`).

#[path = "common/db.rs"]
mod db;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use assistant::ckan::{sync_ckan, Catalogue, CkanReport};
use assistant::crawl::{upsert_site, CrawlPolicy, Crawler, Sink};
use jc_core::kinds::assistant::Visibility;
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use serde_json::{json, Value};
use sqlx::PgPool;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// `data.example` is the mock server; no other name resolves.
struct Catalogue1(u16);

impl Resolve for Catalogue1 {
    fn resolve(&self, name: Name) -> Resolving {
        let found = (name.as_str() == "data.example")
            .then(|| SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), self.0));
        Box::pin(std::future::ready(match found {
            Some(addr) => Ok(Box::new(vec![addr].into_iter()) as Addrs),
            None => Err(format!("the fixture serves no host named {}", name.as_str()).into()),
        }))
    }
}

#[derive(Default)]
struct Pages(Vec<(String, String)>);

impl Sink for Pages {
    async fn page(&mut self, _id: i64, url: &str, _language: Option<&str>, html: &str) {
        self.0.push((url.to_owned(), html.to_owned()));
    }
    async fn document(&mut self, _id: i64, _url: &str, _mime: Option<&str>, _bytes: &[u8]) {}
}

fn dataset(name: &str, modified: &str) -> Value {
    json!({
        "name": name,
        "title": format!("Dataset {name}"),
        "notes": "Readings of the city.",
        "private": false,
        "metadata_modified": modified,
        "resources": [{"name": "Data", "format": "CSV"}]
    })
}

async fn listing(server: &MockServer, start: u64, count: u64, results: Vec<Value>) {
    Mock::given(method("GET"))
        .and(path("/api/3/action/package_search"))
        .and(query_param("start", start.to_string()))
        .and(query_param("include_private", "false"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "success": true,
            "result": {"count": count, "results": results}
        })))
        .mount(server)
        .await;
}

async fn run(
    pool: &PgPool,
    crawler: &Crawler,
    site: i64,
    url: &str,
    max: u32,
) -> (Result<CkanReport, assistant::Error>, Pages) {
    let mut pages = Pages::default();
    let report = sync_ckan(
        pool,
        crawler,
        Catalogue {
            project: "hronov",
            site_id: site,
            url,
            max_datasets: max,
            language: Some("sk"),
        },
        &mut pages,
    )
    .await;
    (report, pages)
}

async fn stored(pool: &PgPool) -> Vec<String> {
    let mut tx = assistant::project_scope(pool, "hronov")
        .await
        .expect("scope");
    let urls = sqlx::query_scalar("SELECT url FROM pages ORDER BY url")
        .fetch_all(&mut *tx)
        .await
        .expect("pages");
    tx.rollback().await.expect("rollback");
    urls
}

#[tokio::test]
async fn public_datasets_are_read_whole_then_by_change_and_removed_when_the_catalogue_drops_them() {
    let (admin, pool, name) = db::database("ckan").await;
    let server = MockServer::start().await;
    let crawler = Crawler::new(
        Arc::new(Catalogue1(server.address().port())),
        CrawlPolicy {
            allow_plain_http: true,
            ..CrawlPolicy::default()
        },
    )
    .expect("crawler");
    let base = format!("http://data.example:{}", server.address().port());
    let site = upsert_site(
        &pool,
        "hronov",
        "hronov.example",
        "catalogue",
        Visibility::Public,
    )
    .await
    .expect("site");

    // First run: 101 public datasets over two listing pages, one private and one whose name
    // would leave the dataset path, both skipped.
    let mut first: Vec<Value> = (0..100)
        .map(|i| dataset(&format!("set-{i:03}"), "2026-10-01T10:00:00"))
        .collect();
    first.push(
        json!({"name": "secret", "private": true, "metadata_modified": "2026-10-01T10:00:00"}),
    );
    listing(&server, 0, 103, first).await;
    listing(
        &server,
        100,
        103,
        vec![
            dataset("set-100", "2026-10-01T10:00:00"),
            json!({"name": "../admin", "metadata_modified": "2026-10-01T10:00:00"}),
        ],
    )
    .await;
    let (report, pages) = run(&pool, &crawler, site, &base, 1_000).await;
    assert_eq!(
        report.expect("first run"),
        CkanReport {
            datasets: 101,
            changed: 101,
            removed: 0
        }
    );
    assert_eq!(pages.0.len(), 101);
    let (url, html) = &pages.0[0];
    assert_eq!(*url, format!("{base}/dataset/set-000"));
    assert!(html.contains("<h1>Dataset set-000</h1>") && html.contains("Data (CSV)"));
    assert!(!stored(&pool)
        .await
        .iter()
        .any(|u| u.contains("secret") || u.contains("admin")));

    // Second run: one dataset changed, one gone; only the changed one is read again.
    server.reset().await;
    let mut second: Vec<Value> = (0..100)
        .filter(|i| *i != 7)
        .map(|i| {
            dataset(
                &format!("set-{i:03}"),
                if i == 3 {
                    "2026-10-06T09:00:00"
                } else {
                    "2026-10-01T10:00:00"
                },
            )
        })
        .collect();
    second.push(dataset("set-100", "2026-10-01T10:00:00"));
    listing(&server, 0, 100, second).await;
    let (report, pages) = run(&pool, &crawler, site, &base, 1_000).await;
    assert_eq!(
        report.expect("second run"),
        CkanReport {
            datasets: 100,
            changed: 1,
            removed: 1
        }
    );
    assert_eq!(
        pages.0.iter().map(|(u, _)| u.as_str()).collect::<Vec<_>>(),
        [format!("{base}/dataset/set-003")]
    );
    let urls = stored(&pool).await;
    assert_eq!(urls.len(), 100);
    assert!(!urls.contains(&format!("{base}/dataset/set-007")));

    // A listing that fails removes nothing.
    server.reset().await;
    Mock::given(method("GET"))
        .and(path("/api/3/action/package_search"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    let (report, _) = run(&pool, &crawler, site, &base, 1_000).await;
    assert!(report
        .expect_err("a failed listing")
        .to_string()
        .contains("500"));
    assert_eq!(stored(&pool).await.len(), 100);

    // A catalogue that answers success: false is refused the same way.
    server.reset().await;
    Mock::given(method("GET"))
        .and(path("/api/3/action/package_search"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"success": false, "error": {"message": "no"}})),
        )
        .mount(&server)
        .await;
    assert!(run(&pool, &crawler, site, &base, 1_000).await.0.is_err());
    assert_eq!(stored(&pool).await.len(), 100);

    // maxPages caps what the source holds; the rest is removed.
    server.reset().await;
    listing(
        &server,
        0,
        2,
        vec![
            dataset("set-000", "2026-10-01T10:00:00"),
            dataset("set-001", "2026-10-01T10:00:00"),
        ],
    )
    .await;
    let (report, _) = run(&pool, &crawler, site, &base, 1).await;
    assert_eq!(
        report.expect("capped"),
        CkanReport {
            datasets: 1,
            changed: 0,
            removed: 99
        }
    );
    db::drop_database(admin, pool, &name).await;
}

#[tokio::test]
async fn a_catalogue_on_a_private_address_is_not_read() {
    let (admin, pool, name) = db::database("ckanprivate").await;
    let site = upsert_site(
        &pool,
        "hronov",
        "hronov.example",
        "catalogue",
        Visibility::Public,
    )
    .await
    .expect("site");
    let crawler = Crawler::public().expect("crawler");
    for url in [
        "https://127.0.0.1/",
        "https://[::1]/",
        "http://data.example/",
    ] {
        let (report, pages) = run(&pool, &crawler, site, url, 10).await;
        assert!(report.is_err(), "{url}");
        assert!(pages.0.is_empty());
    }
    db::drop_database(admin, pool, &name).await;
}
