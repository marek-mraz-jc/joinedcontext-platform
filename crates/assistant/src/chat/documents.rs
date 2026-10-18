//! The search the Context Gateway's `search_documents` calls (API/05 §4, AG-119, EP-133,
//! T-4659): `POST /internal/v1/search` ranks the public passages of the sources an MCP server
//! attaches, with the chat's hybrid search and no model call, and `POST /internal/v1/sources`
//! says what those sources hold, for the tool's description. Cluster network only: a request
//! that came through a proxy is answered as an unknown path, and a NetworkPolicy lets the
//! gateway alone in. No token: nothing but public passages is ever read here.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use axum::extract::rejection::JsonRejection;
use axum::extract::State;
use axum::http::{header, HeaderMap};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;

use super::ChatState;
use crate::Search;

/// The longest query, in characters.
pub const MAX_QUERY: usize = 300;
/// The most sources one call names.
pub const MAX_SOURCES: usize = 20;
/// The most hits one call answers.
pub const MAX_HITS: u8 = 10;
/// The hits a call that names no `limit` answers.
pub const DEFAULT_HITS: u8 = 5;
/// The most of a passage a hit carries, in characters.
pub const MAX_TEXT: usize = 1_200;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceRef {
    project: String,
    source: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Asked {
    query: String,
    sources: Vec<SourceRef>,
    #[serde(default)]
    publisher: Option<String>,
    #[serde(default)]
    lang: Option<String>,
    #[serde(default)]
    limit: Option<u8>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Listed {
    sources: Vec<SourceRef>,
}

pub fn router(state: Arc<ChatState>) -> Router {
    Router::new()
        .route("/internal/v1/search", post(search))
        .route("/internal/v1/sources", post(sources))
        .with_state(state)
}

fn problem(status: u16, detail: impl Into<String>) -> Response {
    jc_core::ProblemDetails::for_status(status)
        .with_detail(detail.into())
        .into_response()
}

/// A request a proxy forwarded came from outside the cluster: the path does not exist there.
fn proxied(headers: &HeaderMap) -> bool {
    headers.contains_key("x-forwarded-for") || headers.contains_key(header::FORWARDED)
}

/// A body that is not one of these requests: the gateway's own mistake, said plainly.
fn unread(rejection: &JsonRejection) -> Response {
    match rejection {
        JsonRejection::MissingJsonContentType(_) => {
            problem(415, "send the body as application/json")
        }
        _ => problem(
            400,
            "the body is not a request of API/05 §4: unknown, missing or mistyped members",
        ),
    }
}

fn unknown() -> Response {
    problem(404, "no such path")
}

fn failed(err: impl std::fmt::Display) -> Response {
    tracing::error!(%err, "a search for an MCP server failed");
    problem(
        503,
        "the assistant's store could not be searched; try again shortly",
    )
}

/// The sources by project, own order kept, a name that is no project or source left out; with
/// `publisher`, only that project's (the publisher of a source is the project that holds it).
fn grouped(sources: &[SourceRef], publisher: Option<&str>) -> Vec<(String, Vec<String>)> {
    let mut grouped: Vec<(String, Vec<String>)> = Vec::new();
    for s in sources {
        if !crate::is_project_name(&s.project)
            || !crate::is_project_name(&s.source)
            || publisher.is_some_and(|p| p != s.project)
        {
            continue;
        }
        match grouped.iter_mut().find(|(p, _)| *p == s.project) {
            Some((_, names)) if names.contains(&s.source) => {}
            Some((_, names)) => names.push(s.source.clone()),
            None => grouped.push((s.project.clone(), vec![s.source.clone()])),
        }
    }
    grouped
}

/// What is wrong with a search request, in words; `None` when nothing is.
fn refused(asked: &Asked) -> Option<String> {
    let length = asked.query.trim().chars().count();
    if length == 0 || length > MAX_QUERY {
        return Some(format!("query must be 1 to {MAX_QUERY} characters"));
    }
    if asked.sources.is_empty() || asked.sources.len() > MAX_SOURCES {
        return Some(format!("sources must name 1 to {MAX_SOURCES} sources"));
    }
    if asked.limit.is_some_and(|l| l == 0 || l > MAX_HITS) {
        return Some(format!("limit must be 1 to {MAX_HITS}"));
    }
    if asked
        .lang
        .as_deref()
        .is_some_and(|l| l.len() != 2 || !l.bytes().all(|b| b.is_ascii_lowercase()))
    {
        return Some("lang must be a two-letter language code".into());
    }
    None
}

/// What the page or document of each hit states of itself, its dates as RFC 3339 instants in UTC.
const DESCRIBED: &str = r#"SELECT c.id, COALESCE(p.title, d.title) AS title,
       COALESCE(p.description, d.description) AS description,
       to_char(COALESCE(p.published, d.published) AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"') AS published,
       to_char(COALESCE(p.modified, d.modified) AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"') AS modified,
       COALESCE(p.language, c.lang) AS language
FROM chunks c LEFT JOIN pages p ON p.id = c.page_id LEFT JOIN documents d ON d.id = c.document_id
WHERE c.id = ANY($1) AND (NOT $2 OR c.visibility = 'public')"#;

async fn search(
    State(state): State<Arc<ChatState>>,
    headers: HeaderMap,
    body: Result<Json<Asked>, JsonRejection>,
) -> Response {
    if proxied(&headers) {
        return unknown();
    }
    let Json(asked) = match body {
        Ok(body) => body,
        Err(rejection) => return unread(&rejection),
    };
    if let Some(why) = refused(&asked) {
        return problem(400, why);
    }
    let scope = grouped(&asked.sources, asked.publisher.as_deref());
    match ranked(
        &state,
        &scope,
        asked.query.trim(),
        asked.lang.as_deref(),
        asked.limit.unwrap_or(DEFAULT_HITS),
        true,
    )
    .await
    {
        Ok(hits) => Json(json!({"hits": hits})).into_response(),
        Err(err) => failed(err),
    }
}

/// The best `limit` passages of `scope` for `query` as hits of API/05 §4, each with what its page
/// or document states of itself; the `public` passages alone when `public_only`.
pub(super) async fn ranked(
    state: &ChatState,
    scope: &[(String, Vec<String>)],
    query: &str,
    lang: Option<&str>,
    limit: u8,
    public_only: bool,
) -> Result<Vec<Value>, crate::Error> {
    if scope.is_empty() {
        return Ok(Vec::new());
    }
    let vector = state.embedder.query(query).await?;
    let hits = crate::search_projects(
        &state.pool,
        scope,
        &Search {
            text: query,
            embedding: &vector,
            sources: &[],
            public_only,
            limit: i64::from(limit),
            lang,
        },
    )
    .await?;
    let about = described(&state.pool, scope, &hits, public_only).await?;
    Ok(hits
        .iter()
        .filter_map(|hit| {
            let about = about.get(&hit.chunk)?;
            Some(json!({
                "title": about.title.clone().or_else(|| crate::heading(&hit.text)),
                "url": hit.url,
                "publisher": about.project,
                "description": about.description,
                "published": about.published,
                "modified": about.modified,
                "language": about.language,
                "text": hit.text.chars().take(MAX_TEXT).collect::<String>(),
                "score": hit.score,
            }))
        })
        .collect())
}

/// What the page or document of a hit states of itself (AG-120).
struct About {
    project: String,
    title: Option<String>,
    description: Option<String>,
    published: Option<String>,
    modified: Option<String>,
    language: Option<String>,
}

/// Each hit's page or document, read in its own project's scope, so the project a chunk is
/// found in is the publisher it is named by.
async fn described(
    pool: &sqlx::PgPool,
    scope: &[(String, Vec<String>)],
    hits: &[crate::Hit],
    public_only: bool,
) -> Result<HashMap<i64, About>, crate::Error> {
    let ids: Vec<i64> = hits.iter().map(|hit| hit.chunk).collect();
    let mut found = HashMap::new();
    if ids.is_empty() {
        return Ok(found);
    }
    for (project, _) in scope {
        let mut tx = crate::project_scope(pool, project).await?;
        let rows = sqlx::query(DESCRIBED)
            .bind(&ids)
            .bind(public_only)
            .fetch_all(&mut *tx)
            .await?;
        tx.rollback().await?;
        for row in rows {
            found.insert(
                row.try_get("id")?,
                About {
                    project: project.clone(),
                    title: row.try_get("title")?,
                    description: row.try_get("description")?,
                    published: row.try_get("published")?,
                    modified: row.try_get("modified")?,
                    language: row.try_get("language")?,
                },
            );
        }
    }
    Ok(found)
}

/// The public passages of the named sources: how many, the projects that publish them, and the
/// share of each language among those whose language is known, rounded to hundredths.
async fn sources(
    State(state): State<Arc<ChatState>>,
    headers: HeaderMap,
    body: Result<Json<Listed>, JsonRejection>,
) -> Response {
    if proxied(&headers) {
        return unknown();
    }
    let Json(listed) = match body {
        Ok(body) => body,
        Err(rejection) => return unread(&rejection),
    };
    if listed.sources.is_empty() || listed.sources.len() > MAX_SOURCES {
        return problem(400, format!("sources must name 1 to {MAX_SOURCES} sources"));
    }
    let mut passages = 0_i64;
    let mut publishers = Vec::new();
    let mut languages: BTreeMap<String, i64> = BTreeMap::new();
    for (project, names) in grouped(&listed.sources, None) {
        let rows: Vec<(Option<String>, i64)> = match async {
            let mut tx = crate::project_scope(&state.pool, &project).await?;
            let rows = sqlx::query_as(
                "SELECT c.lang, count(*)::int8 FROM chunks c JOIN sites s ON s.id = c.site_id \
                 WHERE s.source = ANY($1) AND c.visibility = 'public' GROUP BY c.lang",
            )
            .bind(&names)
            .fetch_all(&mut *tx)
            .await?;
            tx.rollback().await?;
            Ok::<_, crate::Error>(rows)
        }
        .await
        {
            Ok(rows) => rows,
            Err(err) => return failed(err),
        };
        let held: i64 = rows.iter().map(|(_, n)| n).sum();
        if held > 0 {
            passages += held;
            publishers.push(project);
        }
        for (lang, n) in rows {
            if let Some(lang) = lang {
                *languages.entry(lang).or_default() += n;
            }
        }
    }
    let known: i64 = languages.values().sum();
    let shares: BTreeMap<String, f64> = languages
        .into_iter()
        .map(|(lang, n)| {
            #[allow(clippy::cast_precision_loss)]
            let share = (n as f64 / known as f64 * 100.0).round() / 100.0;
            (lang, share)
        })
        .collect();
    Json(json!({"passages": passages, "publishers": publishers, "languages": shares}))
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refs(list: &[(&str, &str)]) -> Vec<SourceRef> {
        list.iter()
            .map(|(p, s)| SourceRef {
                project: (*p).into(),
                source: (*s).into(),
            })
            .collect()
    }

    #[test]
    fn sources_group_by_project_once_each_and_a_publisher_keeps_its_own() {
        let named = refs(&[
            ("banskabystrica", "web"),
            ("bbsk", "web"),
            ("banskabystrica", "web"),
            ("Bad Name", "web"),
            ("bbsk", "../x"),
            ("banskabystrica", "data-portal"),
        ]);
        assert_eq!(
            grouped(&named, None),
            vec![
                (
                    "banskabystrica".into(),
                    vec!["web".into(), "data-portal".into()]
                ),
                ("bbsk".into(), vec!["web".into()]),
            ]
        );
        assert_eq!(
            grouped(&named, Some("bbsk")),
            vec![("bbsk".into(), vec!["web".to_owned()])]
        );
        assert!(grouped(&named, Some("helsinki")).is_empty());
    }

    #[test]
    fn a_query_a_limit_a_language_and_the_sources_are_held_to_their_bounds() {
        let asked = |query: &str, sources: usize, limit: Option<u8>, lang: Option<&str>| Asked {
            query: query.into(),
            sources: refs(&vec![("p", "s"); sources]),
            publisher: None,
            lang: lang.map(Into::into),
            limit,
        };
        assert_eq!(refused(&asked("úradné hodiny", 1, None, Some("sk"))), None);
        assert_eq!(
            refused(&asked(&"á".repeat(MAX_QUERY), 20, Some(10), None)),
            None
        );
        for bad in [
            asked("", 1, None, None),
            asked("   ", 1, None, None),
            asked(&"a".repeat(MAX_QUERY + 1), 1, None, None),
            asked("x", 0, None, None),
            asked("x", 21, None, None),
            asked("x", 1, Some(0), None),
            asked("x", 1, Some(11), None),
            asked("x", 1, None, Some("SK")),
            asked("x", 1, None, Some("slk")),
        ] {
            assert!(refused(&bad).is_some(), "{bad:?}");
        }
    }

    #[test]
    fn a_forwarded_request_came_through_the_edge() {
        let mut headers = HeaderMap::new();
        assert!(!proxied(&headers));
        headers.insert("x-forwarded-for", "203.0.113.9".parse().expect("value"));
        assert!(proxied(&headers));
        let mut headers = HeaderMap::new();
        headers.insert(header::FORWARDED, "for=203.0.113.9".parse().expect("value"));
        assert!(proxied(&headers));
    }
}
