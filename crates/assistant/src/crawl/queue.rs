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
    /// A daily sitemap check, not a full crawl (T-4749).
    pub sitemap_only: bool,
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

/// Enqueues a daily sitemap check of `project`'s website `source` (T-4749).
pub async fn enqueue_check(pool: &PgPool, project: &str, source: &str) -> Result<i64, Error> {
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO crawl_jobs (project, source, state, run_after, attempts, kind) \
         VALUES ($1, $2, 'queued', now(), 0, 'sitemap') \
         RETURNING id",
    )
    .bind(project)
    .bind(source)
    .fetch_one(pool)
    .await?;
    Ok(id)
}

/// Whether `project`'s `source` already has a job queued or running, so a minute's queueing adds
/// no second one.
pub async fn pending(pool: &PgPool, project: &str, source: &str) -> Result<bool, Error> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM crawl_jobs \
         WHERE project = $1 AND source = $2 AND state IN ('queued', 'running'))",
    )
    .bind(project)
    .bind(source)
    .fetch_one(pool)
    .await?)
}

/// How long a claim holds: a job still `running` after it was claimed this long ago lost its
/// worker (a pod an apply replaced mid-crawl) and is claimed again as a further attempt (T-4278).
/// A crawl reads one page at a time with a 30-second timeout, so 800 pages of bbsk.sk take well
/// under an hour.
// ponytail: a fixed lease, no heartbeat; a crawl longer than six hours would be read twice at once.
const LEASE: &str = "6 hours";

/// Claims the next ready queued job using `FOR UPDATE SKIP LOCKED`, or a running one whose lease
/// ran out.
pub async fn claim(pool: &PgPool, worker: &str) -> Result<Option<Job>, Error> {
    // A job that lost its worker five times is failed, as `fail` ends one, so it stops holding
    // the source's later jobs back.
    sqlx::query(
        "UPDATE crawl_jobs \
         SET state = 'failed', finished_at = now(), \
             error = 'the worker stopped during this job five times' \
         WHERE state = 'running' AND attempts >= 5 AND claimed_at < now() - $1::interval",
    )
    .bind(LEASE)
    .execute(pool)
    .await?;
    let row = sqlx::query(
        "UPDATE crawl_jobs \
         SET attempts = attempts + CASE WHEN state = 'running' THEN 1 ELSE 0 END, \
             state = 'running', claimed_by = $1, claimed_at = now(), finished_at = NULL \
         WHERE id = ( \
             SELECT id FROM crawl_jobs \
             WHERE (state = 'queued' AND run_after <= now()) \
                OR (state = 'running' AND claimed_at < now() - $2::interval) \
             ORDER BY run_after, id \
             FOR UPDATE SKIP LOCKED \
             LIMIT 1 \
         ) \
         RETURNING id, project, source, attempts, kind",
    )
    .bind(worker)
    .bind(LEASE)
    .fetch_optional(pool)
    .await?;

    match row {
        Some(r) => Ok(Some(Job {
            id: r.try_get("id")?,
            project: r.try_get("project")?,
            source: r.try_get("source")?,
            attempts: r.try_get("attempts")?,
            sitemap_only: r.try_get::<String, _>("kind")? == "sitemap",
        })),
        None => Ok(None),
    }
}

/// Marks a job as completed.
pub async fn finish(pool: &PgPool, id: i64) -> Result<(), Error> {
    sqlx::query("UPDATE crawl_jobs SET state = 'done', finished_at = now() WHERE id = $1")
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
             error = $2, \
             finished_at = now() \
         WHERE id = $1",
    )
    .bind(id)
    .bind(truncated_err)
    .execute(pool)
    .await?;
    Ok(())
}
