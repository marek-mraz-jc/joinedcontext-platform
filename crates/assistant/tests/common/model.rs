//! The embedding model (`JC_ASSISTANT_TEST_MODEL_DIR`, filled by `scripts/ci/e5-small.sh`) and
//! pages to embed. A missing variable is a failure that says what to set.

use std::path::PathBuf;

use sqlx::PgPool;

/// The model directory the tests load.
pub fn model_dir() -> PathBuf {
    PathBuf::from(
        std::env::var("JC_ASSISTANT_TEST_MODEL_DIR").unwrap_or_else(|_| {
            panic!(
                "set JC_ASSISTANT_TEST_MODEL_DIR to a directory filled by scripts/ci/e5-small.sh"
            )
        }),
    )
}

/// A site of `project` with one page per `(url, lang, text)`, each page one chunk without an
/// embedding; returns the chunk ids.
pub async fn pages(
    pool: &PgPool,
    project: &str,
    source: &str,
    pages: &[(&str, &str, &str)],
) -> Vec<i64> {
    let mut tx = assistant::project_scope(pool, project)
        .await
        .expect("scope");
    let site: i64 = sqlx::query_scalar("INSERT INTO sites (organization, project, source, visibility) VALUES ('example', $1, $2, 'public') RETURNING id")
        .bind(project).bind(source).fetch_one(&mut *tx).await.expect("site");
    let mut ids = Vec::new();
    for (url, lang, text) in pages {
        let page: i64 = sqlx::query_scalar(
            "INSERT INTO pages (site_id, project, url, depth) VALUES ($1, $2, $3, 0) RETURNING id",
        )
        .bind(site)
        .bind(project)
        .bind(*url)
        .fetch_one(&mut *tx)
        .await
        .expect("page");
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO chunks (site_id, project, page_id, ordinal, url, text, lang, visibility) \
             VALUES ($1, $2, $3, 0, $4, $5, $6, 'public') RETURNING id",
        )
        .bind(site)
        .bind(project)
        .bind(page)
        .bind(*url)
        .bind(*text)
        .bind(*lang)
        .fetch_one(&mut *tx)
        .await
        .expect("chunk");
        ids.push(id);
    }
    tx.commit().await.expect("commit");
    ids
}
