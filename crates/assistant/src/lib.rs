//! `jc-assistant`'s store: the migrations of its database and the one hybrid retrieval query
//! (ADR-N-040 §3.3, Architecture/22 §3).
//!
//! Every tenant table answers only for the project a transaction names (row-level security on
//! `jc.project`, forced for the owner too), so the crawl worker and the chat API reach a
//! project's rows through [`project_scope`] and nothing else.

use std::collections::HashMap;
use std::fmt::Write as _;

use sqlx::migrate::Migrator;
use sqlx::postgres::PgRow;
use sqlx::{PgConnection, Postgres, Row, Transaction};

/// The store's migrations, reversible (`*.up.sql` and `*.down.sql`).
pub static MIGRATOR: Migrator = sqlx::migrate!("./migrations");

/// A `catalogue` source: the project's own Endpoints and the models of their spaces.
pub mod catalogue;
/// The chat route: one question, answered as Server-Sent Events (API/05).
pub mod chat;
/// A `ckan` source: a catalogue's public datasets.
pub mod ckan;
/// The knowledge assistant's website crawl core.
pub mod crawl;
/// Passage and question embeddings with `multilingual-e5-small`.
pub mod embed;
/// Text, language and passages out of what the crawl fetched.
pub mod extract;
/// A `guide` source: the platform's User Guide, shipped with this crate.
pub mod guide;
/// When a source is read again: its cron, matched against the minute.
pub mod schedule;
/// The files an administrator uploads to a `source: upload` (T-4184).
pub mod upload;
/// The crawl worker: queueing due sources and working the queue.
pub mod worker;

/// The dimension of every embedding: `multilingual-e5-small` (ADR-N-040 §3.1).
pub const DIMENSIONS: usize = 384;

/// How many candidates each ranking contributes before fusion.
pub const CANDIDATES: i64 = 50;

/// The `k` of reciprocal rank fusion, `1 / (k + rank)`.
pub const RRF_K: i32 = 60;

/// A store error a caller can act on.
#[derive(Debug)]
pub enum Error {
    /// The embedding is not [`DIMENSIONS`] finite numbers.
    Embedding(String),
    /// The project name is not one a manifest could carry.
    Project(String),
    /// The database refused or could not be reached.
    Database(sqlx::Error),
    /// A crawl failure or invalid crawl specification.
    Crawl(String),
    /// A page or document whose text could not be read.
    Extract(String),
    /// The embedding model could not be loaded or run.
    Model(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Embedding(why) => write!(f, "the embedding is not usable: {why}"),
            Error::Project(name) => write!(f, "`{name}` is not a project name"),
            Error::Database(err) => write!(f, "the assistant's database: {err}"),
            Error::Crawl(why) => write!(f, "the crawl failed: {why}"),
            Error::Extract(why) => write!(f, "the text could not be read: {why}"),
            Error::Model(why) => write!(f, "the embedding model: {why}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Database(err) => Some(err),
            _ => None,
        }
    }
}

impl From<sqlx::Error> for Error {
    fn from(err: sqlx::Error) -> Self {
        Error::Database(err)
    }
}

/// A transaction that reads and writes the rows of `project` alone. It ends with the
/// transaction: `set_config(…, true)` is local to it, so a pooled connection never carries one
/// project's scope into the next caller's work.
pub async fn project_scope<'c>(
    pool: &sqlx::PgPool,
    project: &str,
) -> Result<Transaction<'c, Postgres>, Error> {
    if !is_project_name(project) {
        return Err(Error::Project(project.to_owned()));
    }
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT set_config('jc.project', $1, true)")
        .bind(project)
        .execute(&mut *tx)
        .await?;
    Ok(tx)
}

/// A DNS label, which is what a project's `metadata.name` is.
pub(crate) fn is_project_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 63
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !name.starts_with('-')
        && !name.ends_with('-')
}

/// What a question is matched against.
#[derive(Debug, Clone)]
pub struct Search<'a> {
    /// The question, as the person wrote it.
    pub text: &'a str,
    /// Its embedding, [`DIMENSIONS`] numbers.
    pub embedding: &'a [f32],
    /// The `KnowledgeSource` names the deployment answers from.
    pub sources: &'a [String],
    /// A channel nobody signs in to reads public chunks only (MF-52).
    pub public_only: bool,
    /// How many passages come back.
    pub limit: i64,
    /// Passages in this language and those of no known language only; `None` reads every one
    /// (T-4659).
    pub lang: Option<&'a str>,
}

/// One passage, with where it came from and its fused score.
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    /// The chunk's id.
    pub chunk: i64,
    /// The page or document it is from, for the citation.
    pub url: String,
    /// The passage itself.
    pub text: String,
    /// `Σ 1 / (60 + rank)` over the rankings it appears in.
    pub score: f64,
    /// The source it is from, as `{project}/{source}`.
    pub site: String,
    /// When its page or document was published, last changed and last read.
    pub dates: Dates,
}

/// When a passage's page or document was published, last changed and last read by the crawl,
/// each an RFC 3339 instant in UTC; left out where unknown (AG-120, T-4749).
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct Dates {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub published: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modified: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub crawled: Option<String>,
}

/// The pgvector literal of an embedding, refused unless it is [`DIMENSIONS`] finite numbers.
pub fn vector_literal(embedding: &[f32]) -> Result<String, Error> {
    if embedding.len() != DIMENSIONS {
        return Err(Error::Embedding(format!(
            "{} numbers, not {DIMENSIONS}",
            embedding.len()
        )));
    }
    if let Some(at) = embedding.iter().position(|value| !value.is_finite()) {
        return Err(Error::Embedding(format!("number {at} is not finite")));
    }
    let mut literal = String::with_capacity(DIMENSIONS * 10);
    literal.push('[');
    for (i, value) in embedding.iter().enumerate() {
        if i > 0 {
            literal.push(',');
        }
        // `write!` into a String cannot fail.
        let _ = write!(literal, "{value}");
    }
    literal.push(']');
    Ok(literal)
}

/// The hybrid query: full-text and vector rankings of the deployment's sources, fused by
/// reciprocal rank. The question is parsed with every configuration the store indexes with, so
/// it matches a chunk in whatever language that chunk is. Its words are OR-ed: a question asked
/// in a person's words never has all of them in the passage that answers it (AND-ed, the lexical
/// ranking found nothing for any of the forty eval questions, T-3053), and `ts_rank_cd` puts the
/// passages with more of them first. A page under a news path (`/aktuality/`, `/novinky/`,
/// `/news/`, …) counts 0.9 of its score and loses a tie: opening hours, prices and contacts are
/// kept on the pages beside the news, and a news item about them goes stale (a COVID-19 notice
/// answered a pool's hours, T-4370).
///
/// Speed (T-4663, T-5773): under the forced row-level security `@@` is not leakproof, so it is a
/// filter after the policy and no GIN lookup; `question` is materialized, else its four
/// `to_tsvector` calls ran once per chunk of the scope (3.9 s of a 4.1 s search over 35k chunks).
/// The semantic arm is a plain `ORDER BY embedding <=> $2 LIMIT` over `chunks`, its sites an
/// array and not a join, so the HNSW index serves it and filters as it walks ([`KNN_SETTINGS`]);
/// a window over the whole scope sorted every chunk of the project by distance, 23.5 s on prod
/// once one crawl stored 100k chunks.
const HYBRID: &str = r#"
WITH scope AS NOT MATERIALIZED (
    SELECT c.id, c.fts
    FROM chunks c JOIN sites s ON s.id = c.site_id
    WHERE s.source = ANY($3) AND (NOT $4 OR c.visibility = 'public')
      AND ($8::text IS NULL OR c.lang IS NULL OR c.lang = $8)
),
question AS MATERIALIZED (
    SELECT jc_any_word('english', $1) || jc_any_word('finnish', $1)
        || jc_any_word('german', $1) || jc_any_word('jc_simple_unaccent', $1) AS q
),
lexical AS (
    SELECT scope.id, row_number() OVER (ORDER BY ts_rank_cd(scope.fts, question.q) DESC, scope.id) AS rank
    FROM scope, question
    WHERE scope.fts @@ question.q
    ORDER BY rank
    LIMIT $5
),
semantic AS (
    SELECT near.id, row_number() OVER (ORDER BY near.distance, near.id) AS rank
    FROM (
        SELECT c.id, c.embedding <=> $2::vector AS distance
        FROM chunks c
        WHERE c.site_id = ANY (ARRAY(SELECT s.id FROM sites s WHERE s.source = ANY($3)))
          AND (NOT $4 OR c.visibility = 'public')
          AND ($8::text IS NULL OR c.lang IS NULL OR c.lang = $8)
        ORDER BY c.embedding <=> $2::vector
        LIMIT $5
    ) near
)
SELECT c.id, c.url, c.text, site.project || '/' || site.source AS site,
       to_char(COALESCE(p.published, d.published) AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"') AS published,
       to_char(COALESCE(p.modified, d.modified) AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"') AS modified,
       to_char(COALESCE(p.fetched_at, d.fetched_at) AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"') AS crawled,
       ((COALESCE(1.0 / ($6 + lexical.rank), 0) + COALESCE(1.0 / ($6 + semantic.rank), 0))
        * CASE WHEN news.item THEN 0.9 ELSE 1 END)::float8 AS score,
       lexical.rank IS NOT NULL AS by_words, semantic.rank IS NOT NULL AS by_meaning
FROM lexical FULL OUTER JOIN semantic USING (id)
JOIN chunks c ON c.id = COALESCE(lexical.id, semantic.id)
JOIN sites site ON site.id = c.site_id
LEFT JOIN pages p ON p.id = c.page_id
LEFT JOIN documents d ON d.id = c.document_id
CROSS JOIN LATERAL (SELECT c.url ~* '/(aktuality|aktualita|aktualne|novinky|spravy|tlacove-spravy|clanky|news|uutiset|tiedotteet|blog)(/|$|\?|#)' AS item) news
ORDER BY score DESC, news.item, c.id
LIMIT $7
"#;

/// The HNSW walk of the semantic arm, for the transaction it runs in: an iterative scan goes on
/// past `ef_search` until [`CANDIDATES`] chunks pass the scope's filters (pgvector 0.8), where a
/// plain scan returns the 40 nearest of every project and filters them to few or none. The order
/// may be relaxed, since the arm ranks its rows by their distance again; `max_scan_tuples` bounds
/// the walk for a scope that holds few of the chunks.
const KNN_SETTINGS: &str = "SET LOCAL hnsw.ef_search = 100; \
    SET LOCAL hnsw.iterative_scan = relaxed_order; SET LOCAL hnsw.max_scan_tuples = 20000";

/// The passages that answer `search`, best first, read in the transaction's project scope.
pub async fn hybrid_search(
    conn: &mut PgConnection,
    search: &Search<'_>,
) -> Result<Vec<Hit>, Error> {
    let vector = vector_literal(search.embedding)?;
    if search.sources.is_empty() || search.limit <= 0 {
        return Ok(Vec::new());
    }
    sqlx::raw_sql(KNN_SETTINGS).execute(&mut *conn).await?;
    let began = std::time::Instant::now();
    let rows: Vec<PgRow> = sqlx::query(HYBRID)
        .bind(search.text)
        .bind(vector)
        .bind(search.sources)
        .bind(search.public_only)
        .bind(CANDIDATES)
        .bind(RRF_K)
        .bind(search.limit)
        .bind(search.lang)
        .fetch_all(conn)
        .await?;
    // Which arm found each passage kept (T-5779): one statement runs both, so the arms share
    // its time.
    let arm = |column: &str| {
        rows.iter()
            .filter(|row| row.try_get::<bool, _>(column).unwrap_or(false))
            .count()
    };
    tracing::info!(
        sources = ?search.sources,
        rows = rows.len(),
        lexical = arm("by_words"),
        semantic = arm("by_meaning"),
        ms = u64::try_from(began.elapsed().as_millis()).unwrap_or(u64::MAX),
        "search arms"
    );
    rows.iter()
        .map(|row| {
            Ok(Hit {
                chunk: row.try_get("id")?,
                url: row.try_get("url")?,
                text: row.try_get("text")?,
                score: row.try_get("score")?,
                site: row.try_get("site")?,
                dates: Dates {
                    published: row.try_get("published")?,
                    modified: row.try_get("modified")?,
                    crawled: row.try_get("crawled")?,
                },
            })
        })
        .collect::<Result<_, sqlx::Error>>()
        .map_err(Error::from)
}

/// The first Markdown heading of a passage, the title of the page or document it opens: what a
/// citation is named by instead of its address (T-3325). `None` when it opens with none.
pub fn heading(text: &str) -> Option<String> {
    let line = text.lines().map(str::trim).find(|line| !line.is_empty())?;
    let title = line.strip_prefix('#')?.trim_start_matches('#');
    if !title.starts_with(char::is_whitespace) {
        return None;
    }
    let title = title.trim().trim_end_matches('#').trim();
    (!title.is_empty()).then(|| title.chars().take(200).collect())
}

/// The title of each page or document of `urls` the deployment's sources hold, from its first
/// passage (T-3325). Read in the transaction's project scope, so another project's page is never
/// named.
pub async fn titles(
    conn: &mut PgConnection,
    urls: &[String],
    sources: &[String],
    public_only: bool,
) -> Result<HashMap<String, String>, Error> {
    if urls.is_empty() || sources.is_empty() {
        return Ok(HashMap::new());
    }
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT DISTINCT ON (c.url) c.url, c.text FROM chunks c JOIN sites s ON s.id = c.site_id \
         WHERE c.url = ANY($1) AND s.source = ANY($2) AND (NOT $3 OR c.visibility = 'public') \
         ORDER BY c.url, c.ordinal, c.id",
    )
    .bind(urls)
    .bind(sources)
    .bind(public_only)
    .fetch_all(conn)
    .await?;
    Ok(rows
        .into_iter()
        .filter_map(|(url, text)| heading(&text).map(|title| (url, title)))
        .collect())
}

/// The page the deployment's sources hold about an Endpoint: its dataset's page first (a CKAN
/// `…/dataset/{name}` page before any other that names it, T-4729), else the
/// catalogue's page, which both give its address `…/api/endpoint/{slug}` (T-3325); a page whose
/// own URL is an Endpoint address is a JSON file to a visitor and comes last (T-4366). A live-data citation
/// links it, so a visitor reads where the data is described and never a tool's name. A slug that
/// is no DNS label is never looked up. An Endpoint that two or more dataset pages name serves
/// several datasets, and none of their pages is its own: they are left out, so a citation never
/// borrows another type's dataset (T-4747); [`dataset_page`] finds the one for a type.
pub async fn endpoint_page(
    conn: &mut PgConnection,
    slug: &str,
    sources: &[String],
    public_only: bool,
) -> Result<Option<String>, Error> {
    if !is_slug(slug) || sources.is_empty() {
        return Ok(None);
    }
    let url: Option<String> = sqlx::query_scalar(
        "WITH named AS (SELECT c.url, c.id, c.url ~ '/dataset/[^/]+/?$' AS dataset \
             FROM chunks c JOIN sites s ON s.id = c.site_id \
             WHERE s.source = ANY($2) AND (NOT $3 OR c.visibility = 'public') \
             AND c.text ~ ('/api/endpoint/' || $1 || '([^a-z0-9-]|$)')) \
         SELECT url FROM named \
         WHERE NOT dataset OR (SELECT count(DISTINCT url) FROM named WHERE dataset) = 1 \
         ORDER BY NOT dataset, url ~ '/api/endpoint/', id LIMIT 1",
    )
    .bind(slug)
    .bind(sources)
    .bind(public_only)
    .fetch_optional(conn)
    .await?;
    Ok(url)
}

/// A slug the platform could have minted: a DNS label, so it goes into a pattern as itself.
fn is_slug(slug: &str) -> bool {
    !slug.is_empty()
        && slug.len() <= 63
        && slug
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// What a page publishing `kind` from one of `slugs` says (T-4625): the CKAN publisher writes
/// each dataset's NGSI-LD example as `/api/endpoint/{slug}/ngsi-ld/v1/entities?type={kind}`, and
/// a harvested dataset names its type by its OData address `/api/endpoint/{slug}/odata/v4/{kind}`
/// (T-4793: its NGSI-LD link carries no type). `None` for a type name or slug that is not plain,
/// or no slug at all.
fn dataset_pattern(slugs: &[String], kind: &str) -> Option<String> {
    let plain = (1..=100).contains(&kind.len())
        && kind.starts_with(|c: char| c.is_ascii_alphabetic())
        && kind.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
    if !plain || slugs.is_empty() || !slugs.iter().all(|s| is_slug(s)) {
        return None;
    }
    Some(format!(
        "/api/endpoint/({})/(ngsi-ld/v1/entities\\?type=|odata/v4/){kind}([^A-Za-z0-9_]|$)",
        slugs.join("|")
    ))
}

/// The page of `sources`, not an Endpoint's own, that publishes `kind` from one of `slugs`: the
/// CKAN dataset of the Endpoint over the same space that serves the type (T-4625). The newest
/// indexed page wins: a dataset renamed or moved leaves its old page behind in the index until it
/// is swept, and that page answers 404 (T-4711: `/dataset/ovzdusie` beside `public-air`).
pub async fn dataset_page(
    conn: &mut PgConnection,
    slugs: &[String],
    kind: &str,
    sources: &[String],
    public_only: bool,
) -> Result<Option<String>, Error> {
    let Some(pattern) = dataset_pattern(slugs, kind) else {
        return Ok(None);
    };
    if sources.is_empty() {
        return Ok(None);
    }
    let url: Option<String> = sqlx::query_scalar(
        "SELECT c.url FROM chunks c JOIN sites s ON s.id = c.site_id \
         WHERE s.source = ANY($2) AND (NOT $3 OR c.visibility = 'public') \
         AND c.url !~ '/api/endpoint/' AND c.text ~ $1 ORDER BY c.id DESC LIMIT 1",
    )
    .bind(pattern)
    .bind(sources)
    .bind(public_only)
    .fetch_optional(conn)
    .await?;
    Ok(url)
}

/// One CKAN dataset page of the index that publishes a type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeDataset {
    pub class: String,
    pub url: String,
    /// The heading of its first passage; the URL's last segment when it opens with none.
    pub title: String,
}

/// The dataset pages of the project in scope, outside the site `except`, that publish each of
/// `kinds` from the Endpoint `slug` (T-4793): what a catalogue page of an Endpoint with no
/// dataset of its own names per type. Public pages alone when `public_only`.
pub async fn type_datasets(
    conn: &mut PgConnection,
    slug: &str,
    kinds: &[String],
    except: i64,
    public_only: bool,
) -> Result<Vec<TypeDataset>, Error> {
    let mut found = Vec::new();
    for kind in kinds {
        let Some(pattern) = dataset_pattern(&[slug.to_owned()], kind) else {
            continue;
        };
        let rows: Vec<(String, String)> = sqlx::query_as(
            "WITH named AS (SELECT DISTINCT c.url FROM chunks c \
                 WHERE c.site_id <> $2 AND (NOT $3 OR c.visibility = 'public') \
                 AND c.url ~ '/dataset/[^/]+/?$' AND c.url !~ '/api/endpoint/' AND c.text ~ $1) \
             SELECT DISTINCT ON (c.url) c.url, c.text FROM chunks c JOIN named n ON n.url = c.url \
             WHERE c.site_id <> $2 AND (NOT $3 OR c.visibility = 'public') \
             ORDER BY c.url, c.ordinal, c.id",
        )
        .bind(pattern)
        .bind(except)
        .bind(public_only)
        .fetch_all(&mut *conn)
        .await?;
        found.extend(rows.into_iter().map(|(url, text)| TypeDataset {
            class: kind.clone(),
            title: heading(&text).unwrap_or_else(|| {
                url.trim_end_matches('/')
                    .rsplit('/')
                    .next()
                    .unwrap_or_default()
                    .to_owned()
            }),
            url,
        }));
    }
    Ok(found)
}

/// Whether the deployment's sources hold a page at `url`: a dataset an entity names as its
/// `isPartOf` is linked only when a visitor can read it there (T-4793).
pub async fn held_page_projects(
    pool: &sqlx::PgPool,
    url: &str,
    sources: &[(String, Vec<String>)],
    public_only: bool,
) -> Result<bool, Error> {
    for (project, names) in sources {
        let mut tx = project_scope(pool, project).await?;
        let held: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM chunks c JOIN sites s ON s.id = c.site_id \
             WHERE c.url = $1 AND s.source = ANY($2) AND (NOT $3 OR c.visibility = 'public'))",
        )
        .bind(url)
        .bind(names)
        .bind(public_only)
        .fetch_one(&mut *tx)
        .await?;
        tx.rollback().await?;
        if held {
            return Ok(true);
        }
    }
    Ok(false)
}

/// [`dataset_page`] across the projects that hold the sources, in their order.
pub async fn dataset_page_projects(
    pool: &sqlx::PgPool,
    slugs: &[String],
    kind: &str,
    sources: &[(String, Vec<String>)],
    public_only: bool,
) -> Result<Option<String>, Error> {
    if dataset_pattern(slugs, kind).is_none() {
        return Ok(None);
    }
    for (project, names) in sources {
        let mut tx = project_scope(pool, project).await?;
        let page = dataset_page(&mut tx, slugs, kind, names, public_only).await?;
        tx.rollback().await?;
        if page.is_some() {
            return Ok(page);
        }
    }
    Ok(None)
}

/// A deployment's sources by the project that holds them, its own project first: a source named
/// `{project}/{source}` is another project's (T-4281), which the kind admits only from its
/// `scope.projects`. Every lookup below runs in each project's own scope, so the row-level
/// security that keeps a project's rows to itself still holds.
pub fn sources_by_project(own: &str, sources: &[String]) -> Vec<(String, Vec<String>)> {
    let mut grouped: Vec<(String, Vec<String>)> = vec![(own.to_owned(), Vec::new())];
    for source in sources {
        let (project, name) = match source.split_once('/') {
            Some((project, name)) => (project, name),
            None => (own, source.as_str()),
        };
        match grouped.iter_mut().find(|(p, _)| p == project) {
            Some((_, names)) => names.push(name.to_owned()),
            None => grouped.push((project.to_owned(), vec![name.to_owned()])),
        }
    }
    grouped.retain(|(_, names)| !names.is_empty());
    grouped
}

/// The sources a deployment names, `{project}/{source}`, that crawl a website (T-4680): their
/// passages are capped so the catalogue's records and the Endpoints' descriptions come first.
pub fn website_sources(own: &str, named: &[String], sources: &[worker::Source]) -> Vec<String> {
    sources_by_project(own, named)
        .into_iter()
        .flat_map(|(project, names)| names.into_iter().map(move |name| (project.clone(), name)))
        .filter(|(project, name)| {
            sources.iter().any(|s| {
                &s.project == project
                    && &s.name == name
                    && s.spec.source == jc_core::kinds::assistant::SourceType::Website
            })
        })
        .map(|(project, name)| format!("{project}/{name}"))
        .collect()
}

/// [`hybrid_search`] over every project's sources, each ranked in its own project; the best
/// `search.limit` of all by fused score.
pub async fn search_projects(
    pool: &sqlx::PgPool,
    sources: &[(String, Vec<String>)],
    search: &Search<'_>,
) -> Result<Vec<Hit>, Error> {
    let mut hits = Vec::new();
    for (project, names) in sources {
        let mut tx = project_scope(pool, project).await?;
        hits.extend(
            hybrid_search(
                &mut tx,
                &Search {
                    sources: names,
                    ..search.clone()
                },
            )
            .await?,
        );
        tx.rollback().await?;
    }
    hits.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.chunk.cmp(&b.chunk)));
    hits.truncate(usize::try_from(search.limit).unwrap_or(0));
    Ok(hits)
}

/// [`titles`] over every project's sources.
pub async fn titles_projects(
    pool: &sqlx::PgPool,
    urls: &[String],
    sources: &[(String, Vec<String>)],
    public_only: bool,
) -> Result<HashMap<String, String>, Error> {
    let mut found = HashMap::new();
    for (project, names) in sources {
        let mut tx = project_scope(pool, project).await?;
        for (url, title) in titles(&mut tx, urls, names, public_only).await? {
            found.entry(url).or_insert(title);
        }
        tx.rollback().await?;
    }
    Ok(found)
}

/// [`endpoint_page`] over every project's sources, the deployment's own project first.
pub async fn endpoint_page_projects(
    pool: &sqlx::PgPool,
    slug: &str,
    sources: &[(String, Vec<String>)],
    public_only: bool,
) -> Result<Option<String>, Error> {
    for (project, names) in sources {
        let mut tx = project_scope(pool, project).await?;
        let page = endpoint_page(&mut tx, slug, names, public_only).await?;
        tx.rollback().await?;
        if page.is_some() {
            return Ok(page);
        }
    }
    Ok(None)
}

/// The dataset page rows of `kind` read from the Endpoint `own` cite, whatever the deployment
/// (T-4802): a dataset of `own` itself that publishes the type, else one of an Endpoint of
/// `siblings` (its project's Endpoints over the same space), else the one dataset page that
/// names `own`; `None` when there is none, and the answer then links nothing.
pub async fn rows_dataset_projects(
    pool: &sqlx::PgPool,
    own: &str,
    siblings: &[String],
    kind: &str,
    sources: &[(String, Vec<String>)],
    public_only: bool,
) -> Result<Option<String>, Error> {
    let alone = [own.to_owned()];
    if let Some(page) = dataset_page_projects(pool, &alone, kind, sources, public_only).await? {
        return Ok(Some(page));
    }
    let others: Vec<String> = siblings.iter().filter(|s| *s != own).cloned().collect();
    if !others.is_empty() {
        if let Some(page) = dataset_page_projects(pool, &others, kind, sources, public_only).await?
        {
            return Ok(Some(page));
        }
    }
    endpoint_page_projects(pool, own, sources, public_only).await
}

#[cfg(test)]
mod tests {

    #[test]
    fn sources_are_grouped_by_the_project_that_holds_them_own_first() {
        let names = |list: &[&str]| list.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        assert_eq!(
            sources_by_project(
                "banskabystrica",
                &names(&["bbsk/web", "web", "bbsk/data-portal", "catalogue"])
            ),
            vec![
                ("banskabystrica".to_owned(), names(&["web", "catalogue"])),
                ("bbsk".to_owned(), names(&["web", "data-portal"])),
            ]
        );
        assert_eq!(
            sources_by_project("bbsk", &names(&["banskabystrica/web"])),
            vec![("banskabystrica".to_owned(), names(&["web"]))]
        );
        assert!(sources_by_project("bbsk", &[]).is_empty());
    }

    /// T-4680: the website sources a deployment names, its own and another project's, and none
    /// of its catalogue or CKAN sources.
    #[test]
    fn a_deployments_website_sources_are_named_with_their_project() {
        let source = |project: &str, name: &str, spec: serde_json::Value| worker::Source {
            project: project.into(),
            name: name.into(),
            spec: serde_json::from_value(spec).expect("a source spec"),
            ckan_url: None,
            catalogue: Vec::new(),
        };
        let web =
            || serde_json::json!({"source": "website", "startUrls": ["https://www.bbsk.sk/"]});
        let sources = [
            source("bbsk", "web", web()),
            source("banskabystrica", "web", web()),
            source(
                "bbsk",
                "catalogue",
                serde_json::json!({"source": "catalogue"}),
            ),
            source("zilina", "web", web()),
        ];
        let named = ["catalogue", "web", "banskabystrica/web", "praha/web"].map(String::from);
        assert_eq!(
            website_sources("bbsk", &named, &sources),
            ["bbsk/web", "banskabystrica/web"]
        );
        assert!(website_sources("bbsk", &[], &sources).is_empty());
    }
    use super::*;

    /// T-4625: the pattern matches a dataset's NGSI-LD example of the type from a sibling, and
    /// neither another type that starts the same, another slug, nor a name that is not plain.
    #[test]
    fn a_dataset_is_found_by_its_type_from_an_endpoint_of_the_same_space() {
        let slugs = ["bkc4".to_owned(), "w3st".to_owned()];
        let pattern = dataset_pattern(&slugs, "Organization").expect("plain");
        let compiled = regex::Regex::new(&pattern).expect("a pattern");
        let re = |text: &str| compiled.is_match(text);
        assert!(re("curl 'https://api.dev/api/endpoint/bkc4/ngsi-ld/v1/entities?type=Organization&limit=10'"));
        assert!(re(
            "/api/endpoint/w3st/ngsi-ld/v1/entities?type=Organization"
        ));
        assert!(!re(
            "/api/endpoint/bkc4/ngsi-ld/v1/entities?type=OrganizationUnit"
        ));
        assert!(!re(
            "/api/endpoint/other/ngsi-ld/v1/entities?type=Organization"
        ));
        assert!(!re(
            "/api/endpoint/bkc4/ngsi-ld/v1/entitiesXtype=Organization"
        ));
        // T-4793: a harvested dataset names its type by the OData address alone.
        assert!(re(
            "https://api.dev/api/endpoint/bkc4/odata/v4/Organization"
        ));
        assert!(re("/api/endpoint/w3st/odata/v4/Organization?$top=5"));
        assert!(!re("/api/endpoint/bkc4/odata/v4/OrganizationUnit"));
        assert!(!re("/api/endpoint/bkc4/ngsi-ld/v1/entities"));
        assert_eq!(dataset_pattern(&slugs, "Org.*"), None);
        assert_eq!(dataset_pattern(&slugs, ""), None);
        assert_eq!(dataset_pattern(&["a|b".to_owned()], "Organization"), None);
        assert_eq!(dataset_pattern(&[], "Organization"), None);
    }

    /// T-4663: the scope of a search is inlined into both rankings, never materialized.
    #[test]
    fn the_search_scope_is_not_materialized() {
        assert!(HYBRID.contains("WITH scope AS NOT MATERIALIZED ("));
    }

    #[test]
    fn a_passage_is_titled_by_the_heading_it_opens_with() {
        assert_eq!(
            heading("# Helsinki events\n\nWhat happens"),
            Some("Helsinki events".into())
        );
        assert_eq!(
            heading("\n  ## Tapahtumat ##\ntext"),
            Some("Tapahtumat".into())
        );
        assert_eq!(heading("#hashtag is no heading"), None);
        assert_eq!(heading("Text first\n# Later"), None);
        assert_eq!(heading("#  \nx"), None);
        assert_eq!(heading(""), None);
        assert_eq!(
            heading(&format!("# {}", "a".repeat(300))).map(|t| t.len()),
            Some(200)
        );
    }

    #[test]
    fn an_embedding_is_384_finite_numbers() {
        let ok = vec![0.5_f32; DIMENSIONS];
        let literal = vector_literal(&ok).expect("384 numbers");
        assert!(literal.starts_with("[0.5,0.5") && literal.ends_with("0.5]"));
        assert_eq!(literal.matches(',').count(), DIMENSIONS - 1);
        assert!(matches!(
            vector_literal(&ok[..383]),
            Err(Error::Embedding(_))
        ));
        let mut nan = ok.clone();
        nan[7] = f32::NAN;
        assert!(vector_literal(&nan)
            .unwrap_err()
            .to_string()
            .contains("number 7"));
        let mut inf = ok;
        inf[0] = f32::INFINITY;
        assert!(matches!(vector_literal(&inf), Err(Error::Embedding(_))));
    }

    #[test]
    fn a_project_scope_takes_only_a_project_name() {
        for name in ["banskabystrica", "bb-1", "a"] {
            assert!(is_project_name(name), "{name}");
        }
        for name in [
            "",
            "BB",
            "bb_1",
            "-bb",
            "bb-",
            "x' OR '1'='1",
            &"a".repeat(64),
        ] {
            assert!(!is_project_name(name), "{name}");
        }
    }
}
