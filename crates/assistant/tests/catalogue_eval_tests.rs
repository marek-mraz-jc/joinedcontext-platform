//! The catalogue knowledge eval of T-3225 (AG-116): twenty-five questions, five per project, in
//! Slovak, Czech, Finnish and English, over the catalogue pages the five seeded projects of dev
//! give (`fixtures/eval/catalogue.json`, written by `catalogue::page_html` from the deployment
//! seed's Endpoints and models, without counts). A question names every page that answers it:
//! Endpoints over one space share its model, so "what does attribute Y mean" has several right
//! pages. Each page is indexed by the service's own `Indexer` in its own project, and each
//! question searched in its project with the hybrid query, as a public channel asks it. Needs
//! the test database and the model (see `common`); recall prints with `--nocapture`.

#[path = "common/db.rs"]
mod db;
#[path = "common/model.rs"]
mod model;

use std::collections::BTreeMap;

use assistant::crawl::{upsert_site, Sink};
use assistant::embed::{embed_missing, Embedder};
use assistant::extract::Indexer;
use assistant::{hybrid_search, project_scope, Search};
use jc_core::kinds::assistant::Visibility;
use serde_json::Value;

/// Recall@5 of the hybrid query below this fails the build: the catalogue pages got harder to
/// find, or the retrieval got worse.
const RECALL_AT_5_FLOOR: f64 = 0.8;

#[tokio::test]
async fn every_project_finds_the_catalogue_page_that_answers_its_question() {
    let fixture: Value = serde_json::from_str(include_str!("fixtures/eval/catalogue.json"))
        .expect("the catalogue eval is JSON");
    let pages = fixture["pages"].as_array().expect("pages");
    let questions = fixture["questions"].as_array().expect("questions");
    let text = |v: &Value, k: &str| v[k].as_str().expect(k).to_owned();
    assert_eq!(questions.len(), 25);

    let embedder = Embedder::load(&model::model_dir(), 2).expect("the pinned model loads");
    let (admin, pool, name) = db::database("catalogue_eval").await;
    let mut projects: BTreeMap<String, i64> = BTreeMap::new();
    for page in pages {
        let project = text(page, "project");
        let site = match projects.get(&project) {
            Some(site) => *site,
            None => {
                let site = upsert_site(
                    &pool,
                    &project,
                    "example.org",
                    "catalogue",
                    Visibility::Public,
                )
                .await
                .expect("site");
                projects.insert(project.clone(), site);
                site
            }
        };
        let url = text(page, "url");
        let lang = page["lang"].as_str();
        let mut tx = project_scope(&pool, &project).await.expect("scope");
        let page_id: i64 = sqlx::query_scalar(
            "INSERT INTO pages (site_id, project, url, depth, status, included) VALUES ($1, $2, $3, 0, 'fetched', true) RETURNING id",
        )
        .bind(site)
        .bind(&project)
        .bind(&url)
        .fetch_one(&mut *tx)
        .await
        .expect("page");
        tx.commit().await.expect("commit");
        let mut indexer = Indexer::new(pool.clone(), &project, site, "public", 1);
        indexer.page(page_id, &url, lang, &text(page, "html")).await;
        assert!(indexer.failures.is_empty(), "{url}: {:?}", indexer.failures);
    }
    for project in projects.keys() {
        while embed_missing(&pool, &embedder, project, 1_000)
            .await
            .expect("embedded")
            > 0
        {}
    }

    let sources = vec!["catalogue".to_owned()];
    let mut found: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    for question in questions {
        let project = text(question, "project");
        let asked = text(question, "question");
        let expected: Vec<String> = question["expected"]
            .as_array()
            .expect("expected")
            .iter()
            .map(|u| u.as_str().expect("url").to_owned())
            .collect();
        let vector = embedder.query(&asked).await.expect("query");
        let mut tx = project_scope(&pool, &project).await.expect("scope");
        let hits = hybrid_search(
            &mut tx,
            &Search {
                text: &asked,
                embedding: &vector,
                sources: &sources,
                public_only: true,
                limit: 10,
            },
        )
        .await
        .expect("hybrid");
        tx.rollback().await.expect("rollback");
        let mut first: Vec<String> = Vec::new();
        for hit in hits {
            if !first.contains(&hit.url) {
                first.push(hit.url);
            }
        }
        let hit = first.iter().take(5).any(|url| expected.contains(url));
        if !hit {
            println!(
                "miss [{project}] {asked} -> {:?}",
                first.iter().take(5).collect::<Vec<_>>()
            );
        }
        let tally = found.entry(project).or_default();
        tally.0 += usize::from(hit);
        tally.1 += 1;
    }
    let (hits, asked) = found.values().fold((0, 0), |(h, a), (x, y)| (h + x, a + y));
    for (project, (hit, of)) in &found {
        println!("{project:15} recall@5 {hit}/{of}");
    }
    let recall = hits as f64 / asked as f64;
    println!("catalogue recall@5 {recall:.2}");
    assert!(
        recall >= RECALL_AT_5_FLOOR,
        "catalogue recall@5 {recall:.2} < {RECALL_AT_5_FLOOR}"
    );
    db::drop_database(admin, pool, &name).await;
}
