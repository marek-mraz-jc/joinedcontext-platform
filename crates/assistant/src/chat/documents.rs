//! The search the Context Gateway's `search_documents` calls (API/05 §4, AG-119, EP-133,
//! T-4659): `POST /internal/v1/search` ranks the public passages of the sources an MCP server
//! attaches, with the chat's hybrid search and no model call, and `POST /internal/v1/sources`
//! says what those sources hold, for the tool's description; `POST /internal/v1/context` reads
//! the passages around one hit, for `read_knowledge_context` (T-6183). Cluster network only: a request
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
/// The most hits one call answers: the gateway asks one fixed window of them and pages it, so a
/// page never repeats or skips a hit (T-6183).
pub const MAX_HITS: u8 = 50;
/// The hits a call that names no `limit` answers.
pub const DEFAULT_HITS: u8 = 5;
/// The most of a passage a hit carries, in characters.
pub const MAX_TEXT: usize = 1_200;
/// The most path prefixes one call keeps to, and the longest of them, in characters.
pub const MAX_PATHS: usize = 5;
pub const MAX_PATH: usize = 200;
/// The most passages read on each side of a hit (owner 2026-10-31: "10 chunks below, 10 chunks
/// above").
pub const MAX_AROUND: u8 = 10;

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
    /// Passages whose path starts with one of these only (T-6183).
    #[serde(default, rename = "pathPrefix")]
    paths: Vec<String>,
}

/// The passages around one hit (T-6183).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Around {
    sources: Vec<SourceRef>,
    #[serde(rename = "chunkId")]
    chunk: i64,
    before: u8,
    after: u8,
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
        .route("/internal/v1/context", post(context))
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
    if asked.paths.len() > MAX_PATHS || !asked.paths.iter().all(|p| is_path(p)) {
        return Some(format!(
            "pathPrefix holds at most {MAX_PATHS} paths, each starting with / and at most {MAX_PATH} characters"
        ));
    }
    None
}

/// A path prefix: `/` and up to [`MAX_PATH`] characters, none of them a control character. It is
/// compared as text (`^@`), so `%` and `_` are plain characters.
fn is_path(path: &str) -> bool {
    path.starts_with('/') && path.chars().count() <= MAX_PATH && !path.chars().any(char::is_control)
}

/// What the page or document of each hit states of itself, its dates as RFC 3339 instants in UTC.
const DESCRIBED: &str = r#"SELECT c.id, COALESCE(p.title, d.title) AS title,
       COALESCE(p.description, d.description) AS description,
       to_char(COALESCE(p.published, d.published) AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"') AS published,
       to_char(COALESCE(p.modified, d.modified) AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"') AS modified,
       to_char(COALESCE(p.fetched_at, d.fetched_at) AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"') AS crawled,
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
    let query = asked.query.trim();
    let (lang, limit) = (asked.lang.as_deref(), asked.limit.unwrap_or(DEFAULT_HITS));
    // A question that names a publisher reads its pages first, and every source only when they
    // hold no answer; a `publisher` argument has narrowed the scope already.
    let routed = match (&asked.publisher, state.snapshot.read()) {
        (None, Ok(snapshot)) => routed(&scope, query, &snapshot.titles),
        _ => None,
    };
    if scope.is_empty() {
        return Json(json!({"hits": []})).into_response();
    }
    // One embedding serves the routed read and the fallback over every source.
    let vector = match state.embedder.query(query).await {
        Ok(vector) => vector,
        Err(err) => return failed(err),
    };
    if let Some(routed) = routed {
        match ranked(
            &state,
            &routed,
            query,
            &vector,
            (lang, &asked.paths),
            limit,
            true,
        )
        .await
        {
            Ok(hits) if !hits.is_empty() => return Json(json!({"hits": hits})).into_response(),
            Ok(_) => {}
            Err(err) => return failed(err),
        }
    }
    match ranked(
        &state,
        &scope,
        query,
        &vector,
        (lang, &asked.paths),
        limit,
        true,
    )
    .await
    {
        Ok(hits) => Json(json!({"hits": hits})).into_response(),
        Err(err) => failed(err),
    }
}

/// Words of a project's title that say what it is, not where, folded (T-5368).
const GENERIC: &[&str] = &[
    "city",
    "mesto",
    "hlavni",
    "obec",
    "kraj",
    "region",
    "self",
    "governing",
    "samospravny",
    "university",
    "univerzita",
    "municipality",
    "kaupunki",
];

/// The folded words of `text` of four letters or more.
fn words(text: &str) -> Vec<String> {
    jc_core::ngsi_catalog::fold(text)
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| word.chars().count() >= 4)
        .map(str::to_owned)
        .collect()
}

/// How alike a question's word and a name's are: the letters they start with in common, when
/// those are four or more and leave at most two letters of the shorter word, which a Slovak,
/// Czech or Finnish case ending changes ("Žiline", "Helsingin", "Prahy"); else 0.
fn alike(asked: &str, name: &str) -> usize {
    let common = asked
        .chars()
        .zip(name.chars())
        .take_while(|(a, b)| a == b)
        .count();
    let shorter = asked.chars().count().min(name.chars().count());
    if common >= 4 && common + 2 >= shorter {
        common
    } else {
        0
    }
}

/// The part of `scope` whose projects the question names by their own titles or names (T-5368):
/// each word of the question goes to the projects whose name it is most alike, shared when
/// several are, and the projects with the most words win, ties together. `None` when it names
/// none, or every project of the scope. Never wider than `scope`.
fn routed(
    scope: &[(String, Vec<String>)],
    query: &str,
    titles: &BTreeMap<String, Vec<String>>,
) -> Option<Vec<(String, Vec<String>)>> {
    let names: Vec<Vec<String>> = scope
        .iter()
        .map(|(project, _)| {
            let mut names = words(&project.replace('-', " "));
            names.push(project.replace('-', ""));
            for title in titles.get(project).into_iter().flatten() {
                names.extend(
                    words(title)
                        .into_iter()
                        .filter(|w| !GENERIC.contains(&w.as_str())),
                );
            }
            names
        })
        .collect();
    let mut won = vec![0.0_f64; scope.len()];
    for word in words(query) {
        let best: Vec<usize> = names
            .iter()
            .map(|names| {
                names
                    .iter()
                    .map(|name| alike(&word, name))
                    .max()
                    .unwrap_or(0)
            })
            .collect();
        let most = best.iter().copied().max().unwrap_or(0);
        if most == 0 {
            continue;
        }
        let winners = best.iter().filter(|b| **b == most).count();
        for (score, b) in won.iter_mut().zip(&best) {
            if *b == most {
                *score += 1.0 / f64::from(u32::try_from(winners).unwrap_or(u32::MAX));
            }
        }
    }
    let top = won.iter().copied().fold(0.0, f64::max);
    let kept: Vec<(String, Vec<String>)> = scope
        .iter()
        .zip(&won)
        .filter(|(_, score)| top > 0.0 && **score == top)
        .map(|(group, _)| group.clone())
        .collect();
    (!kept.is_empty() && kept.len() < scope.len()).then_some(kept)
}

/// The best `limit` passages of `scope` for `query` (embedded as `vector`) as hits of API/05 §4,
/// each with what its page or document states of itself and where it sits; the `public` passages
/// alone when `public_only`, and of `lang` and under `paths` alone when they are given.
pub(super) async fn ranked(
    state: &ChatState,
    scope: &[(String, Vec<String>)],
    query: &str,
    vector: &[f32],
    (lang, paths): (Option<&str>, &[String]),
    limit: u8,
    public_only: bool,
) -> Result<Vec<Value>, crate::Error> {
    if scope.is_empty() {
        return Ok(Vec::new());
    }
    let found = crate::search_projects(
        &state.pool,
        scope,
        &Search {
            text: query,
            embedding: vector,
            sources: &[],
            public_only,
            limit: i64::from(limit).saturating_mul(i64::from(CANDIDATES_PER_HIT)),
            lang,
            paths,
        },
    )
    .await?;
    let hits = settled(found, usize::from(limit), time::OffsetDateTime::now_utc());
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
                "crawled": about.crawled,
                "language": crate::extract::language_of(&hit.text).map(str::to_owned).or_else(|| about.language.clone()),
                "text": hit.text.chars().take(MAX_TEXT).collect::<String>(),
                "score": hit.score,
                "chunkId": hit.chunk,
                "ordinal": hit.ordinal,
                "path": hit.path,
                "breadcrumb": about.breadcrumb,
                "page": hit.page,
            }))
        })
        .collect())
}

/// How many passages are ranked for each one answered, so the ones [`settled`] drops leave
/// enough behind.
const CANDIDATES_PER_HIT: u8 = 4;

/// How much a passage's score keeps by the age of its page or document (T-5368): whole for a
/// year, then a tenth less a year down to 0.7, so a 2021 waste schedule loses to a current page
/// that matches as well. A page that states no date is a standing page and keeps its score.
fn recency(dates: &crate::Dates, now: time::OffsetDateTime) -> f64 {
    let Some(at) = dates
        .modified
        .as_deref()
        .or(dates.published.as_deref())
        .and_then(|d| {
            time::OffsetDateTime::parse(d, &time::format_description::well_known::Rfc3339).ok()
        })
    else {
        return 1.0;
    };
    let years = (now - at).whole_days() / 365;
    match years {
        ..=0 => 1.0,
        1 => 0.9,
        2 => 0.8,
        _ => 0.7,
    }
}

/// The best `limit` of `hits` to answer (T-5368): a passage of chrome alone (icons, a menu, a
/// share bar, [`chrome_only`](crate::extract::chrome_only)) left out,
/// the score weighed by [`recency`], and one passage per page, its best; an anchor is the same page.
fn settled(hits: Vec<crate::Hit>, limit: usize, now: time::OffsetDateTime) -> Vec<crate::Hit> {
    let mut kept: Vec<crate::Hit> = hits
        .into_iter()
        .filter(|hit| !crate::extract::chrome_only(&hit.text))
        .map(|mut hit| {
            hit.score *= recency(&hit.dates, now);
            hit
        })
        .collect();
    kept.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.chunk.cmp(&b.chunk)));
    let mut pages: Vec<String> = Vec::new();
    kept.retain(|hit| {
        let page = hit.url.split('#').next().unwrap_or(&hit.url).to_owned();
        let first = !pages.contains(&page);
        if first {
            pages.push(page);
        }
        first
    });
    kept.truncate(limit);
    kept
}

/// What the page or document of a hit states of itself (AG-120).
struct About {
    project: String,
    title: Option<String>,
    description: Option<String>,
    published: Option<String>,
    modified: Option<String>,
    crawled: Option<String>,
    language: Option<String>,
    /// The titles of the pages above a website page, by the links the crawl followed to it, the
    /// start page first; empty for a document (T-6183).
    breadcrumb: Vec<String>,
}

/// The titles above each hit's page: its crawl parents up to [`MAX_CRUMBS`], each in the hit's
/// own site, a page without a title by its path.
const CRUMBS: &str = r#"WITH RECURSIVE up (chunk, site, page, hops) AS (
    SELECT c.id, c.site_id, p.parent_id, 1 FROM chunks c JOIN pages p ON p.id = c.page_id
    WHERE c.id = ANY($1) AND (NOT $2 OR c.visibility = 'public')
    UNION ALL
    SELECT up.chunk, up.site, p.parent_id, up.hops + 1 FROM up JOIN pages p ON p.id = up.page
    WHERE p.site_id = up.site AND up.hops < $3
)
SELECT up.chunk, array_agg(COALESCE(p.title, jc_chunk_path(p.url)) ORDER BY up.hops DESC) AS crumbs
FROM up JOIN pages p ON p.id = up.page AND p.site_id = up.site
GROUP BY up.chunk"#;

/// The most pages a breadcrumb climbs.
const MAX_CRUMBS: i32 = 8;

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
        let crumbs: HashMap<i64, Vec<String>> = sqlx::query_as(CRUMBS)
            .bind(&ids)
            .bind(public_only)
            .bind(MAX_CRUMBS)
            .fetch_all(&mut *tx)
            .await?
            .into_iter()
            .collect();
        tx.rollback().await?;
        for row in rows {
            let id: i64 = row.try_get("id")?;
            found.insert(
                id,
                About {
                    project: project.clone(),
                    title: row.try_get("title")?,
                    description: row.try_get("description")?,
                    published: row.try_get("published")?,
                    modified: row.try_get("modified")?,
                    crawled: row.try_get("crawled")?,
                    language: row.try_get("language")?,
                    breadcrumb: crumbs.get(&id).cloned().unwrap_or_default(),
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

/// The passages of one hit's page or document around it, by ordinal: the hit must be a public
/// passage of the named sources, and so must each neighbour, checked one by one, so a public
/// passage beside an internal one never opens it (T-6183).
const AROUND: &str = r#"WITH hit AS (
    SELECT c.site_id, c.page_id, c.document_id, c.ordinal FROM chunks c JOIN sites s ON s.id = c.site_id
    WHERE c.id = $1 AND s.source = ANY($2) AND c.visibility = 'public'
)
SELECT c.id, c.ordinal, c.url, c.text, c.lang, jc_chunk_path(c.url) AS path, jc_chunk_page(c.url) AS page
FROM hit JOIN chunks c ON c.site_id = hit.site_id
    AND (c.page_id = hit.page_id OR c.document_id = hit.document_id)
WHERE c.ordinal BETWEEN hit.ordinal - $3 AND hit.ordinal + $4 AND c.visibility = 'public'
ORDER BY c.ordinal"#;

/// A row of [`AROUND`]: id, ordinal, url, text, lang, path, page.
type AroundRow = (
    i64,
    i32,
    String,
    String,
    Option<String>,
    String,
    Option<i32>,
);

async fn context(
    State(state): State<Arc<ChatState>>,
    headers: HeaderMap,
    body: Result<Json<Around>, JsonRejection>,
) -> Response {
    if proxied(&headers) {
        return unknown();
    }
    let Json(around) = match body {
        Ok(body) => body,
        Err(rejection) => return unread(&rejection),
    };
    if around.sources.is_empty() || around.sources.len() > MAX_SOURCES {
        return problem(400, format!("sources must name 1 to {MAX_SOURCES} sources"));
    }
    if around.before > MAX_AROUND || around.after > MAX_AROUND {
        return problem(400, format!("before and after are 0 to {MAX_AROUND}"));
    }
    for (project, names) in grouped(&around.sources, None) {
        let rows: Vec<AroundRow> = match async {
            let mut tx = crate::project_scope(&state.pool, &project).await?;
            let rows = sqlx::query_as(AROUND)
                .bind(around.chunk)
                .bind(&names)
                .bind(i32::from(around.before))
                .bind(i32::from(around.after))
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
        if rows.is_empty() {
            continue;
        }
        let passages: Vec<Value> = rows
            .into_iter()
            .map(|(id, ordinal, url, text, lang, path, page)| {
                json!({
                    "chunkId": id,
                    "ordinal": ordinal,
                    "url": url,
                    "path": path,
                    "page": page,
                    "language": crate::extract::language_of(&text).map(str::to_owned).or(lang),
                    "text": text.chars().take(MAX_TEXT).collect::<String>(),
                    "hit": id == around.chunk,
                })
            })
            .collect();
        return Json(json!({"publisher": project, "passages": passages})).into_response();
    }
    problem(
        404,
        "no public passage of the attached sources has this chunkId; take one from a search hit",
    )
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
            paths: Vec::new(),
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
            asked("x", 1, Some(MAX_HITS + 1), None),
            asked("x", 1, None, Some("SK")),
            asked("x", 1, None, Some("slk")),
        ] {
            assert!(refused(&bad).is_some(), "{bad:?}");
        }
    }

    /// The hub's projects as the deployment seed titles them (T-5368).
    fn titles() -> BTreeMap<String, Vec<String>> {
        [
            (
                "banskabystrica",
                &["Mesto Banská Bystrica", "City of Banská Bystrica"][..],
            ),
            (
                "bbsk",
                &[
                    "Banskobystrický samosprávny kraj",
                    "Banská Bystrica Self-Governing Region",
                ],
            ),
            (
                "zilina",
                &[
                    "Mesto Žilina a Žilinská univerzita",
                    "City of Žilina and the University of Žilina",
                ],
            ),
            ("praha", &["Hlavní město Praha", "City of Prague"]),
            ("helsinki", &["Helsinki"]),
        ]
        .into_iter()
        .map(|(p, t)| (p.to_owned(), t.iter().map(|t| (*t).to_owned()).collect()))
        .collect()
    }

    fn hub() -> Vec<(String, Vec<String>)> {
        ["banskabystrica", "bbsk", "zilina", "praha", "helsinki"]
            .into_iter()
            .map(|p| (p.to_owned(), vec!["web".to_owned()]))
            .collect()
    }

    #[test]
    fn each_probe_question_reads_the_publisher_it_names_first() {
        let probes: Vec<Value> =
            serde_json::from_str(include_str!("../../tests/fixtures/hub_rag_probes.json"))
                .expect("the probe set is JSON");
        assert_eq!(probes.len(), 30);
        let (scope, titles) = (hub(), titles());
        for probe in &probes {
            let query = probe["query"].as_str().expect("query");
            let routed = routed(&scope, query, &titles).unwrap_or_default();
            let projects: Vec<&str> = routed.iter().map(|(p, _)| p.as_str()).collect();
            assert!(
                projects.contains(&probe["publisher"].as_str().expect("publisher")),
                "{query}: {projects:?}"
            );
            // The city and its region share their name; nothing else is read with them.
            assert!(projects.len() <= 2, "{query}: {projects:?}");
        }
        let only = |query: &str| {
            routed(&scope, query, &titles)
                .map(|r| r.into_iter().map(|(p, _)| p).collect::<Vec<_>>())
        };
        assert_eq!(
            only("BBSK stredné školy prijímacie konanie"),
            Some(vec!["bbsk".to_owned()])
        );
        assert_eq!(
            only("Banskobystrický kraj údržba ciest"),
            Some(vec!["bbsk".to_owned()])
        );
        assert_eq!(
            only("Helsingin kirjastot aukioloajat"),
            Some(vec!["helsinki".to_owned()])
        );
        assert_eq!(
            only("úřední hodiny magistrátu Prahy"),
            Some(vec!["praha".to_owned()])
        );
        assert_eq!(only("parkovanie v Žiline"), Some(vec!["zilina".to_owned()]));
    }

    #[test]
    fn a_question_naming_no_publisher_or_one_outside_the_scope_reads_them_all() {
        let (scope, titles) = (hub(), titles());
        for query in [
            "otváracie hodiny kúpaliska",
            "mesto parkovanie",
            "city library",
            "Bratislava MHD",
        ] {
            assert_eq!(routed(&scope, query, &titles), None, "{query}");
        }
        // Routing narrows, never widens: a scope of one publisher stays as it is.
        let one = vec![("bbsk".to_owned(), vec!["web".to_owned()])];
        assert_eq!(routed(&one, "Helsinki library", &titles), None);
        // A project without a title is named by its own name.
        assert!(routed(&scope, "bbsk", &BTreeMap::new()).is_some());
    }

    fn hit(chunk: i64, url: &str, text: &str, score: f64, modified: Option<&str>) -> crate::Hit {
        crate::Hit {
            chunk,
            url: url.into(),
            text: text.into(),
            score,
            site: "p/web".into(),
            dates: crate::Dates {
                modified: modified.map(Into::into),
                ..crate::Dates::default()
            },
            ..crate::Hit::default()
        }
    }

    #[test]
    fn chrome_goes_one_passage_per_page_stays_and_old_pages_give_way() {
        let now = time::macros::datetime!(2026-10-22 20:00 UTC);
        let prose = "Kúpalisko Aqualand je otvorené denne od 9.00 do 20.00 hod. ".repeat(5);
        // praha.eu's English section: icons, a share bar and a menu, no prose (T-5368).
        let chrome = "![Ikona hodiny](https://praha.eu/o/praha-theme/images/clock.svg)  Share  \
                      ![Ikona Facebook](https://praha.eu/f.svg) [Transport](/t) [Recommendation](/r)";
        let hits = vec![
            hit(
                1,
                "https://praha.eu/web/prague-international/w/a",
                chrome,
                0.05,
                None,
            ),
            hit(2, "https://hel.fi/on-duty#a", &prose, 0.04, None),
            hit(3, "https://hel.fi/on-duty#b", &prose, 0.035, None),
            hit(
                4,
                "https://zilina.sk/odpad-2021",
                &prose,
                0.033,
                Some("2021-03-01T00:00:00Z"),
            ),
            hit(
                5,
                "https://zilina.sk/odpad",
                &prose,
                0.03,
                Some("2026-09-01T00:00:00Z"),
            ),
        ];
        let kept: Vec<i64> = settled(hits, 5, now).iter().map(|h| h.chunk).collect();
        assert_eq!(kept, [2, 5, 4]);
        // The limit holds after the drops.
        let many: Vec<crate::Hit> = (0..8)
            .map(|i| hit(i, &format!("https://x.sk/{i}"), &prose, 1.0, None))
            .collect();
        assert_eq!(settled(many, 5, now).len(), 5);
    }

    #[test]
    fn a_page_keeps_its_score_for_a_year_then_loses_a_tenth_a_year_to_seven_tenths() {
        let now = time::macros::datetime!(2026-10-22 20:00 UTC);
        let at = |modified: Option<&str>, published: Option<&str>| {
            recency(
                &crate::Dates {
                    modified: modified.map(Into::into),
                    published: published.map(Into::into),
                    crawled: Some("2026-10-01T00:00:00Z".into()),
                },
                now,
            )
        };
        assert_eq!(at(None, None), 1.0);
        assert_eq!(at(Some("not a date"), None), 1.0);
        assert_eq!(at(Some("2026-01-01T00:00:00Z"), None), 1.0);
        assert_eq!(at(None, Some("2025-06-01T00:00:00Z")), 0.9);
        assert_eq!(
            at(Some("2024-06-01T00:00:00Z"), Some("2010-01-01T00:00:00Z")),
            0.8
        );
        assert_eq!(at(Some("2021-03-01T00:00:00Z"), None), 0.7);
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
