//! `jc-assistant`'s store: the migrations of its database and the one hybrid retrieval query
//! (ADR-N-040 §3.3, Architecture/22 §3).
//!
//! Every tenant table answers only for the project a transaction names (row-level security on
//! `jc.project`, forced for the owner too), so the crawl worker and the chat API reach a
//! project's rows through [`project_scope`] and nothing else.

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
/// When a source is read again: its cron, matched against the minute.
pub mod schedule;
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
fn is_project_name(name: &str) -> bool {
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
/// it matches a chunk in whatever language that chunk is, and the GIN index still serves it.
/// Its words are OR-ed: a question asked in a person's words never has all of them in the
/// passage that answers it (AND-ed, the lexical ranking found nothing for any of the forty eval
/// questions, T-3053), and `ts_rank_cd` puts the passages with more of them first.
const HYBRID: &str = r#"
WITH scope AS (
    SELECT c.id, c.fts, c.embedding
    FROM chunks c JOIN sites s ON s.id = c.site_id
    WHERE s.source = ANY($3) AND (NOT $4 OR c.visibility = 'public')
),
question AS (
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
    SELECT scope.id, row_number() OVER (ORDER BY scope.embedding <=> $2::vector, scope.id) AS rank
    FROM scope
    WHERE scope.embedding IS NOT NULL
    ORDER BY rank
    LIMIT $5
)
SELECT c.id, c.url, c.text,
       (COALESCE(1.0 / ($6 + lexical.rank), 0) + COALESCE(1.0 / ($6 + semantic.rank), 0))::float8 AS score
FROM lexical FULL OUTER JOIN semantic USING (id)
JOIN chunks c ON c.id = COALESCE(lexical.id, semantic.id)
ORDER BY score DESC, c.id
LIMIT $7
"#;

/// The passages that answer `search`, best first, read in the transaction's project scope.
pub async fn hybrid_search(
    conn: &mut PgConnection,
    search: &Search<'_>,
) -> Result<Vec<Hit>, Error> {
    let vector = vector_literal(search.embedding)?;
    if search.sources.is_empty() || search.limit <= 0 {
        return Ok(Vec::new());
    }
    let rows: Vec<PgRow> = sqlx::query(HYBRID)
        .bind(search.text)
        .bind(vector)
        .bind(search.sources)
        .bind(search.public_only)
        .bind(CANDIDATES)
        .bind(RRF_K)
        .bind(search.limit)
        .fetch_all(conn)
        .await?;
    rows.iter()
        .map(|row| {
            Ok(Hit {
                chunk: row.try_get("id")?,
                url: row.try_get("url")?,
                text: row.try_get("text")?,
                score: row.try_get("score")?,
            })
        })
        .collect::<Result<_, sqlx::Error>>()
        .map_err(Error::from)
}

#[cfg(test)]
mod tests {
    use super::*;

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
