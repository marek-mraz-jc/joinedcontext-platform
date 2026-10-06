//! The retrieval eval of T-3053 (ADR-N-040 §3.3): forty questions in Slovak, Czech, Finnish and
//! English over `fixtures/eval/corpus.json`, recall@5 and recall@10 of the lexical ranking
//! alone, the vector ranking alone and the hybrid query the service runs. The numbers print with
//! `--nocapture`; the floors below are what reopens ADR-N-040 §4 (`pg_search`) when missed.

#[path = "common/db.rs"]
mod db;
#[path = "common/model.rs"]
mod model;

use std::collections::BTreeMap;

use assistant::embed::{embed_missing, Embedder};
use assistant::{hybrid_search, project_scope, vector_literal, Search, CANDIDATES};
use serde_json::Value;
use sqlx::PgPool;

/// Hybrid recall@5 below this fails the build: retrieval got worse.
const HYBRID_RECALL_AT_5_FLOOR: f64 = 0.85;

/// The lexical ranking alone, as the hybrid query's `lexical` part ranks it.
const LEXICAL: &str = r#"
WITH question AS (
    SELECT jc_any_word('english', $1) || jc_any_word('finnish', $1)
        || jc_any_word('german', $1) || jc_any_word('jc_simple_unaccent', $1) AS q
)
SELECT c.url FROM chunks c, question WHERE c.fts @@ question.q
ORDER BY ts_rank_cd(c.fts, question.q) DESC, c.id LIMIT $2
"#;

/// The vector ranking alone, as the hybrid query's `semantic` part ranks it.
const SEMANTIC: &str = "SELECT url FROM chunks WHERE embedding IS NOT NULL \
                        ORDER BY embedding <=> $1::vector, id LIMIT $2";

struct Question {
    lang: String,
    text: String,
    expected: String,
}

fn corpus() -> (Vec<(String, String, String)>, Vec<Question>) {
    let raw: Value = serde_json::from_str(include_str!("fixtures/eval/corpus.json"))
        .expect("the eval corpus is JSON");
    let field = |v: &Value, k: &str| v[k].as_str().expect(k).to_owned();
    let documents = raw["documents"]
        .as_array()
        .expect("documents")
        .iter()
        .map(|d| (field(d, "url"), field(d, "lang"), field(d, "text")))
        .collect();
    let questions = raw["questions"]
        .as_array()
        .expect("questions")
        .iter()
        .map(|q| Question {
            lang: field(q, "lang"),
            text: field(q, "question"),
            expected: field(q, "expected"),
        })
        .collect();
    (documents, questions)
}

async fn ranked(pool: &PgPool, sql: &'static str, bind: String, limit: i64) -> Vec<String> {
    let mut tx = project_scope(pool, "eval").await.expect("scope");
    let urls = sqlx::query_scalar(sql)
        .bind(bind)
        .bind(limit)
        .fetch_all(&mut *tx)
        .await
        .expect("ranking");
    tx.rollback().await.expect("rollback");
    urls
}

/// Share of questions whose page is among the first `k` distinct URLs of their ranking.
fn recall(rankings: &[(usize, Vec<String>)], questions: &[Question], k: usize) -> f64 {
    let found = rankings
        .iter()
        .filter(|(i, urls)| {
            let mut seen: Vec<&String> = Vec::new();
            for url in urls {
                if !seen.contains(&url) {
                    seen.push(url);
                }
            }
            seen.iter()
                .take(k)
                .any(|url| **url == questions[*i].expected)
        })
        .count();
    found as f64 / rankings.len() as f64
}

#[tokio::test]
async fn hybrid_retrieval_finds_the_answering_page_at_least_as_often_as_either_ranking_alone() {
    let (documents, questions) = corpus();
    assert_eq!(documents.len(), 40);
    assert_eq!(questions.len(), 40);
    for question in &questions {
        assert!(
            documents
                .iter()
                .any(|(url, _, _)| *url == question.expected),
            "{} names a page the corpus does not have",
            question.text
        );
    }

    let embedder = Embedder::load(&model::model_dir(), 2).expect("the pinned model loads");
    let (admin, pool, name) = db::database("eval").await;
    let rows: Vec<(&str, &str, &str)> = documents
        .iter()
        .map(|(u, l, t)| (u.as_str(), l.as_str(), t.as_str()))
        .collect();
    model::pages(&pool, "eval", "web", &rows).await;
    assert_eq!(
        embed_missing(&pool, &embedder, "eval", 1_000)
            .await
            .expect("embedded"),
        40
    );

    let sources = vec!["web".to_owned()];
    let mut lexical = Vec::new();
    let mut semantic = Vec::new();
    let mut hybrid: Vec<(usize, Vec<String>)> = Vec::new();
    for (i, question) in questions.iter().enumerate() {
        let vector = embedder.query(&question.text).await.expect("query");
        lexical.push((
            i,
            ranked(&pool, LEXICAL, question.text.clone(), CANDIDATES).await,
        ));
        semantic.push((
            i,
            ranked(
                &pool,
                SEMANTIC,
                vector_literal(&vector).expect("vector"),
                CANDIDATES,
            )
            .await,
        ));
        let mut tx = project_scope(&pool, "eval").await.expect("scope");
        let hits = hybrid_search(
            &mut tx,
            &Search {
                text: &question.text,
                embedding: &vector,
                sources: &sources,
                public_only: true,
                limit: 10,
            },
        )
        .await
        .expect("hybrid");
        tx.rollback().await.expect("rollback");
        hybrid.push((i, hits.into_iter().map(|hit| hit.url).collect()));
    }

    // What a regression looks like, printed with --nocapture: the question and the first pages.
    for (i, urls) in &hybrid {
        if !urls.iter().take(5).any(|u| *u == questions[*i].expected) {
            println!(
                "hybrid miss: {} -> {:?}",
                questions[*i].text,
                urls.iter().take(5).collect::<Vec<_>>()
            );
        }
    }
    let languages = ["sk", "cs", "fi", "en"];
    let by_language = |rankings: &[(usize, Vec<String>)], lang: &str, k: usize| {
        let subset: Vec<(usize, Vec<String>)> = rankings
            .iter()
            .filter(|(i, _)| questions[*i].lang == lang)
            .cloned()
            .collect();
        recall(&subset, &questions, k)
    };
    let mut report = BTreeMap::new();
    for (method, rankings) in [
        ("lexical", &lexical),
        ("vector", &semantic),
        ("hybrid", &hybrid),
    ] {
        for k in [5, 10] {
            let per: Vec<String> = languages
                .iter()
                .map(|lang| format!("{lang} {:.2}", by_language(rankings, lang, k)))
                .collect();
            let all = recall(rankings, &questions, k);
            println!("{method:7} recall@{k:<2} {all:.3}  ({})", per.join(", "));
            report.insert((method, k), all);
        }
    }

    for k in [5, 10] {
        assert!(
            report[&("hybrid", k)] >= report[&("lexical", k)],
            "hybrid recall@{k} fell below lexical alone: {report:?}"
        );
        assert!(
            report[&("hybrid", k)] >= report[&("vector", k)],
            "hybrid recall@{k} fell below vector alone: {report:?}"
        );
    }
    assert!(
        report[&("hybrid", 5)] >= HYBRID_RECALL_AT_5_FLOOR,
        "hybrid recall@5 {:.3} is below the floor {HYBRID_RECALL_AT_5_FLOOR}",
        report[&("hybrid", 5)]
    );
    db::drop_database(admin, pool, &name).await;
}
