//! Text, language and passages out of HTML and PDF, and the indexer that stores them (T-3052).
//!
//! The PDF cases run the real PDFium the crate links (`KREUZBERG_PDFIUM_PREBUILT`, see
//! crates/assistant/README.md); the indexer cases need `JC_ASSISTANT_TEST_DATABASE_URL` like the
//! store tests and fail saying so when it is missing.

use assistant::crawl::Sink;
use assistant::extract::{extract, Indexer, CHUNK_CHARS};
use assistant::{project_scope, MIGRATOR};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{ConnectOptions, PgPool};

/// A one-page PDF whose page shows `text` in Helvetica.
fn pdf(text: &str) -> Vec<u8> {
    pages(&[text])
}

/// A PDF whose page n shows `texts[n]` in Helvetica, with a correct cross-reference table.
fn pages(texts: &[&str]) -> Vec<u8> {
    let kids: Vec<String> = (0..texts.len())
        .map(|n| format!("{} 0 R", 4 + 2 * n))
        .collect();
    let mut objects = vec![
        "<< /Type /Catalog /Pages 2 0 R >>".to_owned(),
        format!(
            "<< /Type /Pages /Kids [{}] /Count {} >>",
            kids.join(" "),
            texts.len()
        ),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_owned(),
    ];
    for (n, text) in texts.iter().enumerate() {
        let stream = format!("BT /F1 18 Tf 72 720 Td ({text}) Tj ET");
        objects.push(format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents {} 0 R /Resources << /Font << /F1 3 0 R >> >> >>", 5 + 2 * n));
        objects.push(format!(
            "<< /Length {} >>\nstream\n{stream}\nendstream",
            stream.len()
        ));
    }
    let mut out = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    for (i, body) in objects.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n{body}\nendobj\n", i + 1).as_bytes());
    }
    let xref = out.len();
    out.extend_from_slice(
        format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
    );
    for offset in offsets {
        out.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            objects.len() + 1
        )
        .as_bytes(),
    );
    out
}

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
    let extracted = extract(html.as_bytes(), "text/html", Some("sk"), 500)
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
        .all(|p| p.text.chars().count() <= CHUNK_CHARS && p.first_page.is_none()));
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
    let extracted = extract(html.as_bytes(), "text/html", None, 500)
        .await
        .expect("html extracts");
    assert_eq!(extracted.language.as_deref(), Some("fi"));
    // A lang attribute that is not a two-letter code is ignored, not trusted.
    let extracted = extract(html.as_bytes(), "text/html", Some("finnish"), 500)
        .await
        .expect("html extracts");
    assert_eq!(extracted.language.as_deref(), Some("fi"));
}

#[tokio::test]
async fn a_heading_or_paragraph_of_whitespace_alone_does_not_lose_the_page() {
    // www.bbsk.sk/uradne-hodiny (T-4688): a block of whitespace alone before a heading, an empty
    // `<h2>&nbsp;</h2>` or the CMS's `<p><strong>&nbsp;<br></strong></p>`, made the HTML converter
    // panic, and the page with the office's hours was never indexed.
    for empty in [
        "<h2>&nbsp;</h2>",
        "<h3> </h3>",
        "<h2><span> </span></h2>",
        "<h4><a href=\"/x\">\u{a0}</a></h4>",
        // The page's real shape: a paragraph of a space in bold before the next heading.
        "<p>Ústredňa BBSK</p><p><strong>&nbsp;<br></strong></p>",
        "<p><em> </em></p>",
    ] {
        let html = format!(
            "<html><body><h1>Úradné hodiny</h1>{empty}<h2>Podateľňa</h2><p>Podateľňa je otvorená v pondelok od 7.30 do 15.30.</p></body></html>"
        );
        let extracted = extract(html.as_bytes(), "text/html", Some("sk"), 500)
            .await
            .unwrap_or_else(|err| panic!("{empty}: {err}"));
        let text: String = extracted.passages.iter().map(|p| p.text.as_str()).collect();
        assert!(text.contains("od 7.30 do 15.30"), "{empty}: {text}");
    }
}

#[tokio::test]
async fn a_pdf_page_becomes_a_passage_that_knows_its_page() {
    let bytes = pdf("Cultural events in Banska Bystrica this summer");
    let extracted = extract(&bytes, "application/pdf", None, 500)
        .await
        .expect("pdf extracts");
    let all: String = extracted.passages.iter().map(|p| p.text.as_str()).collect();
    assert!(
        all.contains("Cultural events in Banska Bystrica"),
        "{all:?}"
    );
    assert_eq!(extracted.passages[0].first_page, Some(1));
    // `pdf.maxPages` 0: nothing of page 1 is kept.
    let capped = extract(&bytes, "application/pdf", None, 0)
        .await
        .expect("pdf extracts");
    assert!(capped.passages.is_empty() && capped.truncated);
}

/// One page whose content stream draws each `(x, y, text)` where it says: a table is its cells.
fn drawn(cells: &[(u32, u32, &str)]) -> Vec<u8> {
    let ops: String = cells
        .iter()
        .map(|(x, y, text)| format!("BT /F1 11 Tf {x} {y} Td ({text}) Tj ET "))
        .collect();
    let stream = ops.trim_end().to_owned();
    let objects = [
        "<< /Type /Catalog /Pages 2 0 R >>".to_owned(),
        "<< /Type /Pages /Kids [4 0 R] /Count 1 >>".to_owned(),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_owned(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 5 0 R /Resources << /Font << /F1 3 0 R >> >> >>".to_owned(),
        format!("<< /Length {} >>\nstream\n{stream}\nendstream", stream.len()),
    ];
    let mut out = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    for (i, body) in objects.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n{body}\nendobj\n", i + 1).as_bytes());
    }
    let xref = out.len();
    out.extend_from_slice(
        format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
    );
    for offset in offsets {
        out.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            objects.len() + 1
        )
        .as_bytes(),
    );
    out
}

/// T-4688: a table of a PDF keeps each row together, so a road's length is read beside the road
/// (www.bbsk.sk's lists of managed roads are tables).
#[tokio::test]
async fn a_pdf_table_keeps_each_row_on_its_line() {
    let rows = [("II/526", "12,4"), ("II/531", "7,9"), ("III/2433", "3,1")];
    let mut cells = vec![(72, 720, "Cesta"), (300, 720, "Dlzka km")];
    for (i, (road, km)) in rows.iter().enumerate() {
        let y = 700 - 20 * u32::try_from(i).unwrap_or(0);
        cells.push((72, y, road));
        cells.push((300, y, km));
    }
    let extracted = extract(&drawn(&cells), "application/pdf", None, 500)
        .await
        .expect("pdf extracts");
    let text: String = extracted
        .passages
        .iter()
        .map(|p| p.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    for (road, km) in rows {
        assert!(
            text.lines().any(|line| line.contains(road)
                && line.contains(km)
                && line.find(road) < line.find(km)),
            "{road} and {km} are not one row: {text}"
        );
    }
}

/// T-3314: a PDF longer than `pdf.maxPages` keeps the passages of its first pages and says it
/// was cut.
#[tokio::test]
async fn a_long_pdf_keeps_its_first_pages() {
    let bytes = pages(&[
        "Prva strana o doprave",
        "Druha strana o skolach",
        "Tretia strana o parkoch",
    ]);
    let extracted = extract(&bytes, "application/pdf", None, 2)
        .await
        .expect("pdf extracts");
    let all: String = extracted.passages.iter().map(|p| p.text.as_str()).collect();
    assert!(
        all.contains("Prva strana") && all.contains("Druha strana"),
        "{all:?}"
    );
    assert!(!all.contains("Tretia"), "{all:?}");
    assert!(extracted.truncated);
    let whole = extract(&bytes, "application/pdf", None, 3)
        .await
        .expect("pdf extracts");
    assert!(!whole.truncated);
}

#[tokio::test]
async fn something_that_is_not_what_it_claims_is_an_error_a_person_can_read() {
    let err = extract(b"not a pdf at all", "application/pdf", None, 500)
        .await
        .expect_err("refused");
    assert!(
        err.to_string()
            .starts_with("the text could not be read: application/pdf"),
        "{err}"
    );
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

/// A site with one page and one document of `project`, their ids.
async fn rows(pool: &PgPool, project: &str) -> (i64, i64, i64) {
    let mut tx = project_scope(pool, project).await.expect("scope");
    let site: i64 = sqlx::query_scalar("INSERT INTO sites (organization, project, source, visibility) VALUES ('bb', $1, 'web', 'public') RETURNING id")
        .bind(project).fetch_one(&mut *tx).await.expect("site");
    let page: i64 = sqlx::query_scalar("INSERT INTO pages (site_id, project, url, depth) VALUES ($1, $2, 'https://bb.example/', 0) RETURNING id")
        .bind(site).bind(project).fetch_one(&mut *tx).await.expect("page");
    let document: i64 = sqlx::query_scalar("INSERT INTO documents (site_id, project, page_id, url) VALUES ($1, $2, $3, 'https://bb.example/plan.pdf') RETURNING id")
        .bind(site).bind(project).bind(page).fetch_one(&mut *tx).await.expect("document");
    tx.commit().await.expect("commit");
    (site, page, document)
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
async fn the_indexer_replaces_a_pages_chunks_and_cites_a_pdfs_page() {
    let (admin, pool, name) = database("indexer").await;
    let (site, page, document) = rows(&pool, "banskabystrica").await;
    let mut indexer = Indexer::new(pool.clone(), "banskabystrica", site, "public", 500);

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

    indexer
        .document(
            document,
            "https://bb.example/plan.pdf",
            Some("application/pdf; charset=binary"),
            &pdf("Zoning plan of the city"),
        )
        .await;
    let all = chunks(&pool, "banskabystrica").await;
    let pdf_chunk = all
        .iter()
        .find(|c| c.1 == Some(document))
        .expect("a document chunk");
    assert_eq!(pdf_chunk.2, "https://bb.example/plan.pdf#page=1");
    assert!(indexer.failures.is_empty(), "{:?}", indexer.failures);
    assert_eq!(indexer.passages, first.len() * 2 + 1);

    // Without a content type and with no .pdf in its address, nothing is guessed.
    indexer
        .document(document, "https://bb.example/download?id=7", None, b"%PDF")
        .await;
    assert_eq!(indexer.failures.len(), 1);
    assert!(
        indexer.failures[0].contains("named no content type"),
        "{:?}",
        indexer.failures
    );
    drop_database(admin, pool, &name).await;
}
