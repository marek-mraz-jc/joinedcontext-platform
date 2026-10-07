//! The Portal's administration of what a project's sources hold (API/05 §3, API/01 §34,
//! T-3057, AG-113): `/internal/v1/projects/{project}/…`, answered to the Portal's service account
//! alone. The edge does not route these paths; the NetworkPolicy lets the Portal in.

use std::collections::HashSet;
use std::sync::Arc;

use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;

use super::ChatState;

/// A refusal, boxed: it is a pointer on the path that serves.
type Refusal = Box<Response>;

/// The most ids one inclusion call may name.
pub const MAX_IDS: usize = 500;

/// The platform's own problem type for `status` (T-3243), never a slug made of a title.
fn problem(status: u16, detail: impl Into<String>) -> Response {
    jc_core::ProblemDetails::for_status(status)
        .with_detail(detail.into())
        .into_response()
}

fn failed(err: impl std::fmt::Display) -> Response {
    tracing::error!(%err, "an administration read failed");
    problem(
        503,
        "The assistant's store could not be read; try again shortly.",
    )
}

/// The administration routes; every one first checks the caller is the Portal.
pub fn router(state: Arc<ChatState>) -> Router {
    Router::new()
        .route(
            "/internal/v1/projects/{project}/knowledge/sources",
            get(sources),
        )
        .route(
            "/internal/v1/projects/{project}/knowledge/sources/{source}/pages",
            get(pages),
        )
        .route(
            "/internal/v1/projects/{project}/knowledge/sources/{source}/documents",
            get(documents),
        )
        .route(
            "/internal/v1/projects/{project}/knowledge/sources/{source}/pages/{page}/links",
            get(links),
        )
        .route(
            "/internal/v1/projects/{project}/knowledge/sources/{source}/passages",
            get(passages),
        )
        .route(
            "/internal/v1/projects/{project}/knowledge/sources/{source}/inclusion",
            post(inclusion),
        )
        .route(
            "/internal/v1/projects/{project}/knowledge/sources/{source}/recrawl",
            post(recrawl),
        )
        .route(
            "/internal/v1/projects/{project}/knowledge/deployments/{deployment}/usage",
            get(usage),
        )
        .with_state(state)
}

/// `Err` unless the bearer is the Portal's service account (AG-113).
pub(super) async fn the_portal(state: &ChatState, headers: &HeaderMap) -> Result<(), Refusal> {
    let Some(bearer) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|v| !v.is_empty())
    else {
        return Err(Box::new(problem(
            401,
            "only the Portal's service account administers the assistant",
        )));
    };
    match state
        .model
        .is_service_account(bearer, &state.portal_client)
        .await
    {
        Ok(true) => Ok(()),
        Ok(false) => Err(Box::new(problem(
            401,
            "only the Portal's service account administers the assistant",
        ))),
        Err(err) => {
            // What failed stays in the log; the caller is told what to do (T-3243, R5).
            tracing::warn!(%err, "the caller's token could not be checked");
            Err(Box::new(problem(
                503,
                "the identity provider could not confirm the caller; try again in a minute",
            )))
        }
    }
}

/// The site row of `source` in `project`, `404` when it has not been crawled.
async fn site(state: &ChatState, project: &str, source: &str) -> Result<i64, Refusal> {
    let mut tx = crate::project_scope(&state.pool, project)
        .await
        .map_err(|err| match err {
            crate::Error::Project(_) => Box::new(problem(404, "no such project")),
            other => Box::new(failed(other)),
        })?;
    let id: Option<i64> =
        sqlx::query_scalar("SELECT id FROM sites WHERE project = $1 AND source = $2")
            .bind(project)
            .bind(source)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|err| Box::new(failed(err)))?;
    tx.rollback().await.map_err(|err| Box::new(failed(err)))?;
    id.ok_or_else(|| {
        Box::new(problem(
            404,
            format!("source `{source}` has not been crawled"),
        ))
    })
}

const UTC: &str = "'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'";

async fn sources(
    State(state): State<Arc<ChatState>>,
    Path(project): Path<String>,
    headers: HeaderMap,
) -> Response {
    if let Err(refused) = the_portal(&state, &headers).await {
        return *refused;
    }
    let Ok(mut tx) = crate::project_scope(&state.pool, &project).await else {
        return problem(404, "no such project");
    };
    let sql = format!(
        "SELECT s.source, s.visibility, to_char(s.last_crawl AT TIME ZONE 'UTC', {UTC}) AS last_crawl, \
           (SELECT count(*) FROM pages p WHERE p.site_id = s.id) AS pages, \
           (SELECT count(*) FROM pages p WHERE p.site_id = s.id AND p.included AND NOT p.excluded_by_admin) AS pages_included, \
           (SELECT count(*) FROM documents d WHERE d.site_id = s.id) AS documents, \
           (SELECT count(*) FROM chunks c WHERE c.site_id = s.id) AS passages, \
           (SELECT count(*) FROM chunks c WHERE c.site_id = s.id AND c.embedding IS NOT NULL) AS embedded \
         FROM sites s ORDER BY s.source"
    );
    let rows = match sqlx::query(sqlx::AssertSqlSafe(sql))
        .fetch_all(&mut *tx)
        .await
    {
        Ok(rows) => rows,
        Err(err) => return failed(err),
    };
    let _ = tx.rollback().await;
    let mut listed = Vec::new();
    for row in rows {
        let source: String = row.try_get("source").unwrap_or_default();
        let job: Option<(String, i32, Option<String>)> = sqlx::query_as(
            "SELECT state, attempts, error FROM crawl_jobs WHERE project = $1 AND source = $2 ORDER BY id DESC LIMIT 1",
        )
        .bind(&project)
        .bind(&source)
        .fetch_optional(&state.pool)
        .await
        .unwrap_or(None);
        listed.push(json!({
            "source": source,
            "state": "crawled",
            "visibility": row.try_get::<String, _>("visibility").unwrap_or_default(),
            "lastCrawl": row.try_get::<Option<String>, _>("last_crawl").unwrap_or(None),
            "pages": row.try_get::<i64, _>("pages").unwrap_or(0),
            "pagesIncluded": row.try_get::<i64, _>("pages_included").unwrap_or(0),
            "documents": row.try_get::<i64, _>("documents").unwrap_or(0),
            "passages": row.try_get::<i64, _>("passages").unwrap_or(0),
            "embedded": row.try_get::<i64, _>("embedded").unwrap_or(0),
            "job": job.map(|(state, attempts, error)| json!({"state": state, "attempts": attempts, "error": error})),
        }));
    }
    Json(json!({"items": listed})).into_response()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Level {
    #[serde(default)]
    parent: Option<i64>,
}

fn excluded_by(included: bool, by_admin: bool) -> Value {
    if by_admin {
        json!("administrator")
    } else if !included {
        json!("pattern")
    } else {
        Value::Null
    }
}

async fn pages(
    State(state): State<Arc<ChatState>>,
    Path((project, source)): Path<(String, String)>,
    Query(level): Query<Level>,
    headers: HeaderMap,
) -> Response {
    if let Err(refused) = the_portal(&state, &headers).await {
        return *refused;
    }
    let site = match site(&state, &project, &source).await {
        Ok(site) => site,
        Err(refused) => return *refused,
    };
    let Ok(mut tx) = crate::project_scope(&state.pool, &project).await else {
        return problem(404, "no such project");
    };
    let sql = format!(
        "SELECT p.id, p.url, p.depth::int AS depth, p.parent_id, p.status, p.included, p.excluded_by_admin, p.language, \
           to_char(p.fetched_at AT TIME ZONE 'UTC', {UTC}) AS fetched_at, \
           (SELECT count(*) FROM pages c WHERE c.parent_id = p.id) AS children, \
           (SELECT count(*) FROM documents d WHERE d.page_id = p.id) AS documents, \
           (SELECT count(*) FROM chunks k WHERE k.page_id = p.id) AS passages \
         FROM pages p WHERE p.site_id = $1 AND p.parent_id IS NOT DISTINCT FROM $2 ORDER BY p.url LIMIT 1000"
    );
    let rows = match sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(site)
        .bind(level.parent)
        .fetch_all(&mut *tx)
        .await
    {
        Ok(rows) => rows,
        Err(err) => return failed(err),
    };
    let _ = tx.rollback().await;
    let items: Vec<Value> = rows
        .iter()
        .map(|row| {
            let included: bool = row.try_get("included").unwrap_or(true);
            let by_admin: bool = row.try_get("excluded_by_admin").unwrap_or(false);
            json!({
                "id": row.try_get::<i64, _>("id").unwrap_or_default(),
                "url": row.try_get::<String, _>("url").unwrap_or_default(),
                "depth": row.try_get::<i32, _>("depth").unwrap_or_default(),
                "parentId": row.try_get::<Option<i64>, _>("parent_id").unwrap_or(None),
                "status": row.try_get::<String, _>("status").unwrap_or_default(),
                "included": included && !by_admin,
                "excludedBy": excluded_by(included, by_admin),
                "language": row.try_get::<Option<String>, _>("language").unwrap_or(None),
                "fetchedAt": row.try_get::<Option<String>, _>("fetched_at").unwrap_or(None),
                "children": row.try_get::<i64, _>("children").unwrap_or(0),
                "documents": row.try_get::<i64, _>("documents").unwrap_or(0),
                "passages": row.try_get::<i64, _>("passages").unwrap_or(0),
            })
        })
        .collect();
    Json(json!({"items": items})).into_response()
}

async fn documents(
    State(state): State<Arc<ChatState>>,
    Path((project, source)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    if let Err(refused) = the_portal(&state, &headers).await {
        return *refused;
    }
    let site = match site(&state, &project, &source).await {
        Ok(site) => site,
        Err(refused) => return *refused,
    };
    let Ok(mut tx) = crate::project_scope(&state.pool, &project).await else {
        return problem(404, "no such project");
    };
    let rows = match sqlx::query(
        "SELECT d.id, d.url, d.page_id, d.mime, d.bytes, d.pages, d.off_domain, d.status, d.included, d.excluded_by_admin, \
           (SELECT count(*) FROM chunks k WHERE k.document_id = d.id) AS passages \
         FROM documents d WHERE d.site_id = $1 ORDER BY d.url LIMIT 2000",
    )
    .bind(site)
    .fetch_all(&mut *tx)
    .await
    {
        Ok(rows) => rows,
        Err(err) => return failed(err),
    };
    let _ = tx.rollback().await;
    let items: Vec<Value> = rows
        .iter()
        .map(|row| {
            let included: bool = row.try_get("included").unwrap_or(true);
            let by_admin: bool = row.try_get("excluded_by_admin").unwrap_or(false);
            json!({
                "id": row.try_get::<i64, _>("id").unwrap_or_default(),
                "url": row.try_get::<String, _>("url").unwrap_or_default(),
                "pageId": row.try_get::<Option<i64>, _>("page_id").unwrap_or(None),
                "mime": row.try_get::<Option<String>, _>("mime").unwrap_or(None),
                "bytes": row.try_get::<Option<i64>, _>("bytes").unwrap_or(None),
                "pages": row.try_get::<Option<i32>, _>("pages").unwrap_or(None),
                "offDomain": row.try_get::<bool, _>("off_domain").unwrap_or(false),
                "status": row.try_get::<String, _>("status").unwrap_or_default(),
                "included": included && !by_admin,
                "excludedBy": excluded_by(included, by_admin),
                "passages": row.try_get::<i64, _>("passages").unwrap_or(0),
            })
        })
        .collect();
    Json(json!({"items": items})).into_response()
}

async fn links(
    State(state): State<Arc<ChatState>>,
    Path((project, source, page)): Path<(String, String, i64)>,
    headers: HeaderMap,
) -> Response {
    if let Err(refused) = the_portal(&state, &headers).await {
        return *refused;
    }
    let site = match site(&state, &project, &source).await {
        Ok(site) => site,
        Err(refused) => return *refused,
    };
    let Ok(mut tx) = crate::project_scope(&state.pool, &project).await else {
        return problem(404, "no such project");
    };
    let found: Option<i64> =
        match sqlx::query_scalar("SELECT id FROM pages WHERE id = $1 AND site_id = $2")
            .bind(page)
            .bind(site)
            .fetch_optional(&mut *tx)
            .await
        {
            Ok(found) => found,
            Err(err) => return failed(err),
        };
    if found.is_none() {
        return problem(404, "no such page in this source");
    }
    let rows: Vec<(String, String)> = match sqlx::query_as(
        "SELECT to_url, kind FROM links WHERE from_page = $1 ORDER BY to_url LIMIT 1000",
    )
    .bind(page)
    .fetch_all(&mut *tx)
    .await
    {
        Ok(rows) => rows,
        Err(err) => return failed(err),
    };
    let _ = tx.rollback().await;
    Json(json!({"items": rows.into_iter().map(|(url, kind)| json!({"url": url, "kind": kind})).collect::<Vec<_>>()})).into_response()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Owner {
    #[serde(default)]
    page: Option<i64>,
    #[serde(default)]
    document: Option<i64>,
}

async fn passages(
    State(state): State<Arc<ChatState>>,
    Path((project, source)): Path<(String, String)>,
    Query(owner): Query<Owner>,
    headers: HeaderMap,
) -> Response {
    if let Err(refused) = the_portal(&state, &headers).await {
        return *refused;
    }
    let site = match site(&state, &project, &source).await {
        Ok(site) => site,
        Err(refused) => return *refused,
    };
    if owner.page.is_some() == owner.document.is_some() {
        return problem(400, "name one page or one document: ?page= or ?document=");
    }
    let Ok(mut tx) = crate::project_scope(&state.pool, &project).await else {
        return problem(404, "no such project");
    };
    let rows: Vec<(i32, String, Option<String>, String)> = match sqlx::query_as(
        "SELECT ordinal, text, lang, url FROM chunks WHERE site_id = $1 \
         AND page_id IS NOT DISTINCT FROM $2 AND document_id IS NOT DISTINCT FROM $3 ORDER BY ordinal LIMIT 200",
    )
    .bind(site)
    .bind(owner.page)
    .bind(owner.document)
    .fetch_all(&mut *tx)
    .await
    {
        Ok(rows) => rows,
        Err(err) => return failed(err),
    };
    let _ = tx.rollback().await;
    let items: Vec<Value> = rows
        .into_iter()
        .map(|(ordinal, text, lang, url)| json!({"ordinal": ordinal, "text": text, "lang": lang, "url": url}))
        .collect();
    Json(json!({"items": items})).into_response()
}

/// The body of `inclusion` (API/01 §34).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Inclusion {
    #[serde(default)]
    pub pages: Vec<i64>,
    #[serde(default)]
    pub documents: Vec<i64>,
    #[serde(default)]
    pub subtree: bool,
    pub included: bool,
}

async fn inclusion(
    State(state): State<Arc<ChatState>>,
    Path((project, source)): Path<(String, String)>,
    headers: HeaderMap,
    body: Result<Json<Inclusion>, JsonRejection>,
) -> Response {
    if let Err(refused) = the_portal(&state, &headers).await {
        return *refused;
    }
    let Json(change) = match body {
        Ok(body) => body,
        Err(rejection) => return super::body_refused(&rejection),
    };
    if change.pages.is_empty() && change.documents.is_empty() {
        return problem(400, "name at least one page or document");
    }
    if change.pages.len() + change.documents.len() > MAX_IDS {
        return problem(
            400,
            format!("at most {MAX_IDS} pages and documents in one call"),
        );
    }
    let site = match site(&state, &project, &source).await {
        Ok(site) => site,
        Err(refused) => return *refused,
    };
    match apply_inclusion(&state.pool, &project, site, &change).await {
        Ok(counts) => Json(counts).into_response(),
        Err(err) => failed(err),
    }
}

/// Sets the administrator's choice on the pages (with their subtrees when asked, and the
/// documents those pages link) and the documents of `site`; excluding removes their passages,
/// including clears their validators so the next crawl indexes them (AG-113).
pub async fn apply_inclusion(
    pool: &sqlx::PgPool,
    project: &str,
    site: i64,
    change: &Inclusion,
) -> Result<Value, crate::Error> {
    let pages: Vec<i64> = change
        .pages
        .iter()
        .copied()
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let mut tx = crate::project_scope(pool, project).await?;
    let pages: Vec<i64> = if change.subtree {
        sqlx::query_scalar(
            "WITH RECURSIVE tree AS (SELECT id FROM pages WHERE site_id = $1 AND id = ANY($2) \
             UNION SELECT p.id FROM pages p JOIN tree t ON p.parent_id = t.id WHERE p.site_id = $1) SELECT id FROM tree",
        )
        .bind(site)
        .bind(&pages)
        .fetch_all(&mut *tx)
        .await?
    } else {
        sqlx::query_scalar("SELECT id FROM pages WHERE site_id = $1 AND id = ANY($2)")
            .bind(site)
            .bind(&pages)
            .fetch_all(&mut *tx)
            .await?
    };
    let documents: Vec<i64> = sqlx::query_scalar(
        "SELECT id FROM documents WHERE site_id = $1 AND (id = ANY($2) OR ($3 AND page_id = ANY($4)))",
    )
    .bind(site)
    .bind(&change.documents)
    .bind(change.subtree)
    .bind(&pages)
    .fetch_all(&mut *tx)
    .await?;
    let reset = if change.included {
        ", content_hash = NULL, etag = NULL, last_modified = NULL"
    } else {
        ""
    };
    // `reset` is one of two literals above, never input.
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE pages SET excluded_by_admin = $2{reset} WHERE id = ANY($1)"
    )))
    .bind(&pages)
    .bind(!change.included)
    .execute(&mut *tx)
    .await?;
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE documents SET excluded_by_admin = $2{reset} WHERE id = ANY($1)"
    )))
    .bind(&documents)
    .bind(!change.included)
    .execute(&mut *tx)
    .await?;
    let removed = if change.included {
        0
    } else {
        sqlx::query("DELETE FROM chunks WHERE page_id = ANY($1) OR document_id = ANY($2)")
            .bind(&pages)
            .bind(&documents)
            .execute(&mut *tx)
            .await?
            .rows_affected()
    };
    tx.commit().await?;
    Ok(json!({"pages": pages.len(), "documents": documents.len(), "passagesRemoved": removed}))
}

async fn recrawl(
    State(state): State<Arc<ChatState>>,
    Path((project, source)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    if let Err(refused) = the_portal(&state, &headers).await {
        return *refused;
    }
    let declared = state
        .snapshot
        .read()
        .map(|s| {
            s.sources
                .iter()
                .any(|x| x.project == project && x.name == source)
        })
        .unwrap_or(false);
    if !declared {
        return problem(
            404,
            format!("project {project} declares no KnowledgeSource `{source}`"),
        );
    }
    match crate::crawl::queue::pending(&state.pool, &project, &source).await {
        Ok(true) => return problem(409, "a crawl of this source is already queued or running"),
        Ok(false) => {}
        Err(err) => return failed(err),
    }
    match crate::crawl::queue::enqueue(&state.pool, &project, &source).await {
        Ok(job) => (StatusCode::ACCEPTED, Json(json!({"job": job}))).into_response(),
        Err(err) => failed(err),
    }
}

async fn usage(
    State(state): State<Arc<ChatState>>,
    Path((project, deployment)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    if let Err(refused) = the_portal(&state, &headers).await {
        return *refused;
    }
    let Ok(mut tx) = crate::project_scope(&state.pool, &project).await else {
        return problem(404, "no such project");
    };
    let rows: Vec<(String, i64, i64, i64)> = match sqlx::query_as(
        "SELECT to_char(day, 'YYYY-MM-DD'), requests, tokens_in, tokens_out FROM usage \
         WHERE deployment = $1 AND day > current_date - 30 ORDER BY day",
    )
    .bind(&deployment)
    .fetch_all(&mut *tx)
    .await
    {
        Ok(rows) => rows,
        Err(err) => return failed(err),
    };
    let _ = tx.rollback().await;
    let items: Vec<Value> = rows
        .into_iter()
        .map(|(day, requests, input, output)| json!({"day": day, "requests": requests, "tokensIn": input, "tokensOut": output}))
        .collect();
    Json(json!({"items": items})).into_response()
}
