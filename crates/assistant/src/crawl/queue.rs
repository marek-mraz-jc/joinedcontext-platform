//! The global crawl job queue (`crawl_jobs`, T-3052).
//!
//! Unlike tenant tables, `crawl_jobs` is shared across workers and is not subject to RLS.

use sqlx::{PgPool, Row};

use crate::Error;

/// A claimed crawl job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    /// Job ID in `crawl_jobs`.
    pub id: i64,
    /// The project name.
    pub project: String,
    /// The source name.
    pub source: String,
    /// Number of attempts made so far.
    pub attempts: i32,
}

/// Enqueues a crawl job for `project` and `source`.
pub async fn enqueue(pool: &PgPool, project: &str, source: &str) -> Result<i64, Error> {
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO crawl_jobs (project, source, state, run_after, attempts) \
         VALUES ($1, $2, 'queued', now(), 0) \
         RETURNING id",
    )
    .bind(project)
    .bind(source)
    .fetch_one(pool)
    .await?;
    Ok(id)
}

/// Claims the next ready queued job using `FOR UPDATE SKIP LOCKED`.
pub async fn claim(pool: &PgPool, worker: &str) -> Result<Option<Job>, Error> {
    let row = sqlx::query(
        "UPDATE crawl_jobs \
         SET state = 'running', claimed_by = $1, claimed_at = now() \
         WHERE id = ( \
             SELECT id FROM crawl_jobs \
             WHERE state = 'queued' AND run_after <= now() \
             ORDER BY run_after, id \
             FOR UPDATE SKIP LOCKED \
             LIMIT 1 \
         ) \
         RETURNING id, project, source, attempts",
    )
    .bind(worker)
    .fetch_optional(pool)
    .await?;

    match row {
        Some(r) => Ok(Some(Job {
            id: r.try_get("id")?,
            project: r.try_get("project")?,
            source: r.try_get("source")?,
            attempts: r.try_get("attempts")?,
        })),
        None => Ok(None),
    }
}

/// Marks a job as completed.
pub async fn finish(pool: &PgPool, id: i64) -> Result<(), Error> {
    sqlx::query("UPDATE crawl_jobs SET state = 'done' WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Records a job failure, scheduling exponential retry or marking as `failed` at 5 attempts.
pub async fn fail(pool: &PgPool, id: i64, error: &str) -> Result<(), Error> {
    let truncated_err: String = error.chars().take(2000).collect();
    sqlx::query(
        "UPDATE crawl_jobs \
         SET attempts = attempts + 1, \
             state = CASE WHEN attempts + 1 >= 5 THEN 'failed' ELSE 'queued' END, \
             run_after = CASE \
                 WHEN attempts + 1 >= 5 THEN run_after \
                 ELSE now() + LEAST((power(2, attempts + 1) || ' minutes')::interval, interval '6 hours') \
             END, \
             error = $2 \
         WHERE id = $1",
    )
    .bind(id)
    .bind(truncated_err)
    .execute(pool)
    .await?;
    Ok(())
}
