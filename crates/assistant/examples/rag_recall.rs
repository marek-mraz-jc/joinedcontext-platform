//! The retrieval half of a website eval (T-4688): indexes a corpus of fetched pages and
//! documents with the service's own `Indexer`, embeds it with the pinned model, and searches each
//! question of an eval file with the chat's first search round (the hybrid query, as a public
//! channel asks it). It writes the ranked addresses per question, which
//! `joinedcontext-conformance/tests/assistant/eval_bbsk_web.py --retrieval` turns into recall@5
//! and MRR, and prints what each source became (passages, PDF pages) to stderr.
//!
//! ```text
//! JC_ASSISTANT_TEST_DATABASE_URL=postgres://… JC_ASSISTANT_TEST_MODEL_DIR=… \
//!   cargo run -p assistant --example rag_recall -- corpus.json bbsk_web_questions.json ranked.json [queries.json]
//! ```
//!
//! `corpus.json` is `[{"url", "file", "mime"}]`, the files as the site served them.
//! `queries.json`, `{"<question id>": ["<query>", …]}`, adds the searches the router would write
//! beside a question; each is searched as the chat searches it and the rankings are fused as the
//! chat fuses them ([`merge`], [`top`]). The run creates a database of its own on the server and
//! drops it at the end.

use std::collections::BTreeMap;
use std::error::Error;

use assistant::chat::routed::{merge, top};
use assistant::crawl::{upsert_site, Sink};
use assistant::embed::{embed_missing, Embedder};
use assistant::extract::Indexer;
use assistant::{hybrid_search, project_scope, Hit, Search, CANDIDATES, MIGRATOR};
use jc_core::kinds::assistant::Visibility;
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::ConnectOptions;

const PROJECT: &str = "eval";
const SOURCE: &str = "web";
/// The `pdf.maxPages` of the dev source the eval measures.
const PDF_PAGES: u32 = 200;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    url: String,
    file: String,
    mime: String,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (corpus, questions, out, queries) = match args.as_slice() {
        [corpus, questions, out] => (corpus, questions, out, BTreeMap::new()),
        [corpus, questions, out, queries] => (
            corpus,
            questions,
            out,
            serde_json::from_str::<BTreeMap<String, Vec<String>>>(&std::fs::read_to_string(
                queries,
            )?)?,
        ),
        _ => {
            return Err(
                "usage: rag_recall CORPUS.json QUESTIONS.json OUT.json [QUERIES.json]".into(),
            )
        }
    };
    let corpus: Vec<Entry> = serde_json::from_str(&std::fs::read_to_string(corpus)?)?;
    let questions: Vec<Value> = serde_json::from_str(&std::fs::read_to_string(questions)?)?;
    let url = std::env::var("JC_ASSISTANT_TEST_DATABASE_URL")?;
    let model = std::env::var("JC_ASSISTANT_TEST_MODEL_DIR")?;

    let admin_options: PgConnectOptions = url.parse()?;
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect_with(admin_options.clone())
        .await?;
    let name = format!("rag_recall_{}", std::process::id());
    // The name is this program's word and its process id, never input.
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE {name}")))
        .execute(&admin)
        .await?;
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect_with(admin_options.database(&name).disable_statement_logging())
        .await?;
    let ranked = measure(&pool, &model, &corpus, &questions, &queries).await;
    pool.close().await;
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP DATABASE {name}")))
        .execute(&admin)
        .await?;
    std::fs::write(out, serde_json::to_string_pretty(&ranked?)?)?;
    Ok(())
}

async fn measure(
    pool: &sqlx::PgPool,
    model: &str,
    corpus: &[Entry],
    questions: &[Value],
    queries: &BTreeMap<String, Vec<String>>,
) -> Result<Value, Box<dyn Error>> {
    MIGRATOR.run(pool).await?;
    let embedder = Embedder::load(std::path::Path::new(model), 4)?;
    let site = upsert_site(pool, PROJECT, "eval", SOURCE, Visibility::Public).await?;
    let mut indexer = Indexer::new(pool.clone(), PROJECT, site, "public", PDF_PAGES);
    for entry in corpus {
        let bytes = std::fs::read(&entry.file)?;
        let mut tx = project_scope(pool, PROJECT).await?;
        let before = indexer.passages;
        eprintln!("indexing {}", entry.url);
        if entry.mime == "text/html" {
            let id: i64 = sqlx::query_scalar(
                "INSERT INTO pages (site_id, project, url, depth, status, included) \
                 VALUES ($1, $2, $3, 1, 'fetched', true) RETURNING id",
            )
            .bind(site)
            .bind(PROJECT)
            .bind(&entry.url)
            .fetch_one(&mut *tx)
            .await?;
            tx.commit().await?;
            indexer
                .page(id, &entry.url, Some("sk"), &String::from_utf8_lossy(&bytes))
                .await;
        } else {
            let id: i64 = sqlx::query_scalar(
                "INSERT INTO documents (site_id, project, url, mime, status) \
                 VALUES ($1, $2, $3, $4, 'fetched') RETURNING id",
            )
            .bind(site)
            .bind(PROJECT)
            .bind(&entry.url)
            .bind(&entry.mime)
            .fetch_one(&mut *tx)
            .await?;
            tx.commit().await?;
            indexer
                .document(id, &entry.url, Some(&entry.mime), &bytes)
                .await;
        }
        eprintln!("{} passages {}", entry.url, indexer.passages - before);
    }
    for failure in &indexer.failures {
        eprintln!("failed {failure}");
    }
    eprintln!(
        "indexed {} sources, {} passages, {} cut at a cap",
        corpus.len(),
        indexer.passages,
        indexer.truncated
    );
    while embed_missing(pool, &embedder, PROJECT, 1_000).await? > 0 {}

    let sources = vec![SOURCE.to_owned()];
    let mut ranked = BTreeMap::new();
    for question in questions {
        let (Some(id), Some(asked)) = (question["id"].as_str(), question["q"].as_str()) else {
            continue;
        };
        let mut found: Vec<Hit> = Vec::new();
        let extra = queries.get(id).map_or(&[][..], Vec::as_slice);
        for text in std::iter::once(asked).chain(extra.iter().map(String::as_str)) {
            let vector = embedder.query(text).await?;
            let mut tx = project_scope(pool, PROJECT).await?;
            let hits = hybrid_search(
                &mut tx,
                &Search {
                    text,
                    embedding: &vector,
                    sources: &sources,
                    public_only: true,
                    limit: CANDIDATES,
                    lang: None,
                },
            )
            .await?;
            tx.rollback().await?;
            merge(&mut found, hits);
        }
        let mut urls: Vec<String> = Vec::new();
        for hit in top(&found, found.len()) {
            if !urls.contains(&hit.url) {
                urls.push(hit.url);
            }
        }
        ranked.insert(id.to_owned(), json!(urls));
    }
    Ok(json!(ranked))
}
