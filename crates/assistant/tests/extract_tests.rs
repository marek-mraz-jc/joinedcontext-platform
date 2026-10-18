//! Text, language and passages out of HTML and text, and the indexer that stores them (T-3052);
//! no PDF is read (T-4524).
//!
//! The indexer cases need `JC_ASSISTANT_TEST_DATABASE_URL` like the store tests and fail saying
//! so when it is missing.

use assistant::crawl::Sink;
use assistant::extract::{extract, Indexer, CHUNK_CHARS};
use assistant::{project_scope, MIGRATOR};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{ConnectOptions, PgPool};

fn long_slovak_page() -> String {
    // Numbered, so every sentence is distinct and an overlap is visible.
    let sentences: String = (1..=40)
        .map(|n| format!("Veta {n}: mesto Banská Bystrica pripravilo nové cyklotrasy, ktoré spájajú centrum s obcami. "))
        .collect();
    format!("<html lang=\"sk\"><body><h1>Cyklotrasy</h1><p>{sentences}</p></body></html>")
}

#[tokio::test]
async fn a_long_page_becomes_overlapping_passages_in_its_declared_language() {
    let html = long_slovak_page();
    let extracted = extract(html.as_bytes(), "text/html", Some("sk"))
        .await
        .expect("html extracts");
    assert_eq!(extracted.language.as_deref(), Some("sk"));
    assert!(
        extracted.passages.len() >= 3,
        "{} passages",
        extracted.passages.len()
    );
    assert!(extracted
        .passages
        .iter()
        .all(|p| p.text.chars().count() <= CHUNK_CHARS));
    assert!(!extracted.truncated);
    // Overlap: the last whole sentence of one passage reappears in the next.
    let first = &extracted.passages[0].text;
    let last_sentence = first.rsplit("Veta ").next().expect("a sentence");
    assert!(
        extracted.passages[1].text.contains(last_sentence.trim()),
        "no overlap: {last_sentence:?}"
    );
    // No passage is a heading alone, and each carries its heading.
    assert!(
        extracted.passages.iter().all(|p| p.text.contains("Veta ")),
        "a heading-only passage was kept"
    );
    assert!(
        extracted
            .passages
            .iter()
            .all(|p| p.text.contains("Cyklotrasy")),
        "a passage lost its heading context"
    );
}

#[tokio::test]
async fn an_undeclared_language_is_detected() {
    let html = "<html><body><p>Helsingin kaupunki avaa uusia pyöräteitä keskustan ja lähiöiden välille. \
                Kaupunkipyörät ovat käytössä huhtikuusta lokakuuhun, ja asemia on yli kolmesataa.</p></body></html>";
    let extracted = extract(html.as_bytes(), "text/html", None)
        .await
        .expect("html extracts");
    assert_eq!(extracted.language.as_deref(), Some("fi"));
    // A lang attribute that is not a two-letter code is ignored, not trusted.
    let extracted = extract(html.as_bytes(), "text/html", Some("finnish"))
        .await
        .expect("html extracts");
    assert_eq!(extracted.language.as_deref(), Some("fi"));
}

/// T-4524: the build carries no PDF or office extractor, so a PDF handed to `extract` is an
/// error a person can read, never text.
#[tokio::test]
async fn a_pdf_is_not_read() {
    let err = extract(b"%PDF-1.4\n%%EOF\n", "application/pdf", None)
        .await
        .expect_err("no PDF extractor");
    assert!(
        err.to_string()
            .starts_with("the text could not be read: application/pdf"),
        "{err}"
    );
}

#[tokio::test]
async fn csv_and_markdown_are_read_as_text() {
    let csv = extract(
        "obec;suma\nBanská Bystrica;1200\n".as_bytes(),
        "text/csv",
        Some("sk"),
    )
    .await
    .expect("csv extracts");
    let all: String = csv.passages.iter().map(|p| p.text.as_str()).collect();
    assert!(all.contains("Banská Bystrica"), "{all:?}");
    let md = extract(
        "# Návod\n\nPrihláška sa podáva v podateľni.\n".as_bytes(),
        "text/markdown",
        Some("sk"),
    )
    .await
    .expect("markdown extracts");
    let all: String = md.passages.iter().map(|p| p.text.as_str()).collect();
    assert!(all.contains("podateľni"), "{all:?}");
}

async fn database(test: &str) -> (PgPool, PgPool, String) {
    let url = std::env::var("JC_ASSISTANT_TEST_DATABASE_URL").unwrap_or_else(|_| {
        panic!("set JC_ASSISTANT_TEST_DATABASE_URL to a PostgreSQL with pgvector whose user may create databases (see tests/store_tests.rs)")
    });
    let options: PgConnectOptions = url.parse().expect("the URL parses");
    let admin = PgPoolOptions::new()
        .max_connections(2)
        .connect_with(options.clone())
        .await
        .expect("the test server answers");
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let name = format!("assistant_{test}_{}_{nanos}", std::process::id());
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE {name}")))
        .execute(&admin)
        .await
        .expect("create");
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect_with(options.database(&name).disable_statement_logging())
        .await
        .expect("connect");
    MIGRATOR.run(&pool).await.expect("migrate");
    (admin, pool, name)
}

async fn drop_database(admin: PgPool, pool: PgPool, name: &str) {
    pool.close().await;
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "DROP DATABASE IF EXISTS {name} WITH (FORCE)"
    )))
    .execute(&admin)
    .await
    .expect("drop");
}

/// A site with one page of `project`, their ids.
async fn rows(pool: &PgPool, project: &str) -> (i64, i64) {
    let mut tx = project_scope(pool, project).await.expect("scope");
    let site: i64 = sqlx::query_scalar("INSERT INTO sites (organization, project, source, visibility) VALUES ('bb', $1, 'web', 'public') RETURNING id")
        .bind(project).fetch_one(&mut *tx).await.expect("site");
    let page: i64 = sqlx::query_scalar("INSERT INTO pages (site_id, project, url, depth) VALUES ($1, $2, 'https://bb.example/', 0) RETURNING id")
        .bind(site).bind(project).fetch_one(&mut *tx).await.expect("page");
    tx.commit().await.expect("commit");
    (site, page)
}

async fn chunks(
    pool: &PgPool,
    project: &str,
) -> Vec<(Option<i64>, Option<i64>, String, Option<String>, String)> {
    let mut tx = project_scope(pool, project).await.expect("scope");
    let found = sqlx::query_as("SELECT page_id, document_id, url, lang, visibility FROM chunks ORDER BY page_id NULLS LAST, ordinal")
        .fetch_all(&mut *tx).await.expect("chunks");
    tx.rollback().await.expect("rollback");
    found
}

#[tokio::test]
async fn the_indexer_replaces_a_pages_chunks() {
    let (admin, pool, name) = database("indexer").await;
    let (site, page) = rows(&pool, "banskabystrica").await;
    let mut indexer = Indexer::new(pool.clone(), "banskabystrica", site, "public");

    indexer
        .page(page, "https://bb.example/", Some("sk"), &long_slovak_page())
        .await;
    let first = chunks(&pool, "banskabystrica").await;
    assert!(
        first.len() >= 3
            && first
                .iter()
                .all(|c| c.0 == Some(page) && c.3.as_deref() == Some("sk") && c.4 == "public"),
        "{first:?}"
    );

    // The same page again: its chunks are replaced, never added to.
    indexer
        .page(page, "https://bb.example/", Some("sk"), &long_slovak_page())
        .await;
    assert_eq!(chunks(&pool, "banskabystrica").await.len(), first.len());

    let all = chunks(&pool, "banskabystrica").await;
    assert!(all.iter().all(|c| c.2 == "https://bb.example/"), "{all:?}");
    assert!(indexer.failures.is_empty(), "{:?}", indexer.failures);
    assert_eq!(indexer.passages, first.len() * 2);
    drop_database(admin, pool, &name).await;
}
