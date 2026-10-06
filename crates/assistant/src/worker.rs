//! The crawl worker of `jc-assistant` (Architecture/22 §1, T-3052): once a minute it reads the
//! `KnowledgeSource` manifests from the organization's checkout, queues every website source
//! whose schedule names this minute (or that was never read), and works the queue one job at a
//! time: claim, crawl, extract, index, done or retried.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use jc_core::kinds::assistant::{KnowledgeSourceSpec, SourceType};
use sqlx::PgPool;

use crate::crawl::{crawl_site, queue, Crawler};
use crate::extract::Indexer;
use crate::{schedule, Error};

/// One `KnowledgeSource` the repository declares.
#[derive(Debug, Clone)]
pub struct Source {
    pub project: String,
    pub name: String,
    pub spec: KnowledgeSourceSpec,
}

/// Where the manifests are: the organization's checkout and, in layout 2, the project
/// checkouts and the directory they are assembled in (the gateway's own arrangement).
#[derive(Debug, Clone)]
pub struct Checkout {
    pub organization: PathBuf,
    pub projects: Option<PathBuf>,
    pub assembly: PathBuf,
}

/// Every `KnowledgeSource` of the organization, by project and name; a manifest that does not
/// read as one is logged and left out, so one typo never stops the other sources.
pub fn sources(checkout: &Checkout) -> Result<Vec<Source>, Error> {
    let mut directories = BTreeMap::new();
    if let Some(projects) = &checkout.projects {
        if let Ok(listing) = std::fs::read_dir(projects) {
            for entry in listing.flatten().filter(|entry| entry.path().is_dir()) {
                if let Ok(slug) = entry.file_name().into_string() {
                    if !slug.starts_with('.') {
                        directories.insert(slug, entry.path());
                    }
                }
            }
        }
    }
    let assembly = jcctl::assemble::assemble(
        &checkout.organization,
        &jcctl::assemble::Directories(directories),
        &checkout.assembly,
        None,
    )
    .map_err(|err| Error::Crawl(format!("the manifests could not be read: {err}")))?;
    let mut found = Vec::new();
    for (id, resource) in assembly.repository.iter() {
        if id.kind != "KnowledgeSource" {
            continue;
        }
        let Some(project) = id.namespace.clone() else {
            continue;
        };
        match serde_json::from_value::<KnowledgeSourceSpec>(resource.manifest.spec.clone()) {
            Ok(spec) => found.push(Source {
                project,
                name: id.name.clone(),
                spec,
            }),
            Err(err) => {
                tracing::warn!(project = %project, source = %id.name, %err, "not a KnowledgeSource spec")
            }
        }
    }
    Ok(found)
}

/// Whether a website source is due in the minute `now`: never read, or its schedule names it.
pub fn due(
    spec: &KnowledgeSourceSpec,
    last_crawl: Option<time::OffsetDateTime>,
    now: time::OffsetDateTime,
) -> bool {
    if spec.source != SourceType::Website {
        return false;
    }
    let Some(last) = last_crawl else { return true };
    // Read at most once a minute, whatever a schedule of `* * * * *` says.
    if now - last < time::Duration::minutes(1) {
        return false;
    }
    schedule::matches(spec.schedule.as_deref().unwrap_or(schedule::DEFAULT), now).unwrap_or(false)
}

/// When the source was last read, `None` before its first crawl.
async fn last_crawl(
    pool: &PgPool,
    project: &str,
    source: &str,
) -> Result<Option<time::OffsetDateTime>, Error> {
    let mut tx = crate::project_scope(pool, project).await?;
    // As seconds, so the store needs no date type of sqlx's beyond what it has.
    let at: Option<Option<i64>> = sqlx::query_scalar(
        "SELECT extract(epoch FROM last_crawl)::bigint FROM sites WHERE project = $1 AND source = $2",
    )
    .bind(project)
    .bind(source)
    .fetch_optional(&mut *tx)
    .await?;
    tx.rollback().await?;
    Ok(at
        .flatten()
        .and_then(|seconds| time::OffsetDateTime::from_unix_timestamp(seconds).ok()))
}

/// One minute's queueing: every due source not already queued or running gets a job.
pub async fn enqueue_due(
    pool: &PgPool,
    sources: &[Source],
    now: time::OffsetDateTime,
) -> Result<usize, Error> {
    let mut queued = 0;
    for source in sources {
        if !due(
            &source.spec,
            last_crawl(pool, &source.project, &source.name).await?,
            now,
        ) {
            continue;
        }
        if queue::pending(pool, &source.project, &source.name).await? {
            continue;
        }
        queue::enqueue(pool, &source.project, &source.name).await?;
        queued += 1;
    }
    Ok(queued)
}

/// Claims and works one job; `false` when the queue had none ready.
pub async fn work_one(
    pool: &PgPool,
    crawler: &Crawler,
    organization: &str,
    worker: &str,
    sources: &[Source],
) -> Result<bool, Error> {
    let Some(job) = queue::claim(pool, worker).await? else {
        return Ok(false);
    };
    let Some(source) = sources
        .iter()
        .find(|source| source.project == job.project && source.name == job.source)
    else {
        // The manifest went away while the job waited: nothing to read, nothing to retry.
        queue::finish(pool, job.id).await?;
        return Ok(true);
    };
    let site = crate::crawl::upsert_site(
        pool,
        &source.project,
        organization,
        &source.name,
        source.spec.visibility,
    )
    .await?;
    let visibility = match source.spec.visibility {
        jc_core::kinds::assistant::Visibility::Public => "public",
        jc_core::kinds::assistant::Visibility::Internal => "internal",
    };
    let mut indexer = Indexer::new(
        pool.clone(),
        &source.project,
        site,
        visibility,
        source.spec.pdf.max_pages,
    );
    match crawl_site(
        pool,
        crawler,
        organization,
        &source.project,
        &source.name,
        &source.spec,
        &mut indexer,
    )
    .await
    {
        Ok(report) => {
            tracing::info!(
                project = %source.project, source = %source.name,
                pages = report.pages_fetched, unchanged = report.pages_unchanged,
                documents = report.documents_fetched, passages = indexer.passages,
                failures = indexer.failures.len(), "crawled"
            );
            queue::finish(pool, job.id).await?;
        }
        Err(err) => {
            tracing::warn!(project = %source.project, source = %source.name, %err, "crawl failed, retried later");
            queue::fail(pool, job.id, &err.to_string()).await?;
        }
    }
    Ok(true)
}

/// The organization checkout of a deployment: `JC_ASSISTANT_REPO_DIR`, `JC_ASSISTANT_PROJECTS_DIR`
/// and `JC_ASSISTANT_ASSEMBLY_DIR` (default the system temporary directory).
pub fn checkout_from_env(lookup: impl Fn(&str) -> Option<String>) -> Option<Checkout> {
    let organization = PathBuf::from(lookup("JC_ASSISTANT_REPO_DIR")?);
    Some(Checkout {
        organization,
        projects: lookup("JC_ASSISTANT_PROJECTS_DIR").map(PathBuf::from),
        assembly: lookup("JC_ASSISTANT_ASSEMBLY_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| Path::new(&std::env::temp_dir()).join("jc-assistant-assembly")),
    })
}
