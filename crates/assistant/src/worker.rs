//! The crawl worker of `jc-assistant` (Architecture/22 §1, T-3052): once a minute it reads the
//! `KnowledgeSource` manifests from the organization's checkout, queues every source whose
//! schedule names this minute (or that was never read), and works the queue one job at a time:
//! claim, crawl a website or read a catalogue (T-3054), extract, index, done or retried.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use jc_core::kinds::assistant::{AssistantDeploymentSpec, KnowledgeSourceSpec, SourceType};
use jc_core::kinds::ckan::CkanInstanceSpec;
use sqlx::PgPool;

use crate::ckan::{sync_ckan, Catalogue};
use crate::crawl::{crawl_site, queue, Crawler};
use crate::extract::Indexer;
use crate::{schedule, Error};

/// One `KnowledgeSource` the repository declares.
#[derive(Debug, Clone)]
pub struct Source {
    pub project: String,
    pub name: String,
    pub spec: KnowledgeSourceSpec,
    /// For a `ckan` source, the URL of the project's `CkanInstance` it names; `None` when the
    /// project declares no such instance.
    pub ckan_url: Option<String>,
}

/// Where the manifests are: the organization's checkout and, in layout 2, the project
/// checkouts and the directory they are assembled in (the gateway's own arrangement).
#[derive(Debug, Clone)]
pub struct Checkout {
    pub organization: PathBuf,
    pub projects: Option<PathBuf>,
    pub assembly: PathBuf,
}

/// One `AssistantDeployment` of the organization (T-3055).
#[derive(Debug, Clone)]
pub struct Deployment {
    pub project: String,
    pub name: String,
    pub spec: AssistantDeploymentSpec,
}

/// What a connector needs of the Endpoint it names: where its MCP surface is and who may call it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointRef {
    pub slug: String,
    /// The Endpoint's `spec.audience`: `public`, `organization` or `project-list`.
    pub audience: String,
}

/// The manifests the service works from, read in one pass every minute.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub sources: Vec<Source>,
    pub deployments: Vec<Deployment>,
    /// Every Endpoint, by project and name.
    pub endpoints: BTreeMap<(String, String), EndpointRef>,
}

/// Every `KnowledgeSource` of the organization, by project and name; a manifest that does not
/// read as one is logged and left out, so one typo never stops the other sources.
pub fn sources(checkout: &Checkout) -> Result<Vec<Source>, Error> {
    snapshot(checkout).map(|snapshot| snapshot.sources)
}

/// The sources, the deployments and the Endpoints of the organization's checkout.
pub fn snapshot(checkout: &Checkout) -> Result<Snapshot, Error> {
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
    let mut catalogues = BTreeMap::new();
    for (id, resource) in assembly.repository.iter() {
        if id.kind != "CkanInstance" {
            continue;
        }
        if let (Some(project), Ok(spec)) = (
            id.namespace.clone(),
            serde_json::from_value::<CkanInstanceSpec>(resource.manifest.spec.clone()),
        ) {
            catalogues.insert((project, id.name.clone()), spec.url);
        }
    }
    let mut found = Vec::new();
    for (id, resource) in assembly.repository.iter() {
        if id.kind != "KnowledgeSource" {
            continue;
        }
        let Some(project) = id.namespace.clone() else {
            continue;
        };
        match serde_json::from_value::<KnowledgeSourceSpec>(resource.manifest.spec.clone()) {
            Ok(spec) => {
                let ckan_url = spec
                    .ckan_instance_ref
                    .as_ref()
                    .and_then(|name| catalogues.get(&(project.clone(), name.clone())).cloned());
                found.push(Source {
                    project,
                    name: id.name.clone(),
                    spec,
                    ckan_url,
                })
            }
            Err(err) => {
                tracing::warn!(project = %project, source = %id.name, %err, "not a KnowledgeSource spec")
            }
        }
    }
    let mut snapshot = Snapshot {
        sources: found,
        ..Snapshot::default()
    };
    for (id, resource) in assembly.repository.iter() {
        let Some(project) = id.namespace.clone() else {
            continue;
        };
        let spec = &resource.manifest.spec;
        match id.kind.as_str() {
            "Endpoint" => {
                if let (Some(slug), Some(audience)) = (
                    spec.get("slug").and_then(|v| v.as_str()),
                    spec.get("audience").and_then(|v| v.as_str()),
                ) {
                    snapshot.endpoints.insert(
                        (project, id.name.clone()),
                        EndpointRef {
                            slug: slug.to_owned(),
                            audience: audience.to_owned(),
                        },
                    );
                }
            }
            "AssistantDeployment" => {
                match serde_json::from_value::<AssistantDeploymentSpec>(spec.clone())
                    .map_err(|err| err.to_string())
                    .and_then(|spec| {
                        spec.validate()
                            .map(|()| spec)
                            .map_err(|err| err.to_string())
                    }) {
                    Ok(spec) => snapshot.deployments.push(Deployment {
                        project,
                        name: id.name.clone(),
                        spec,
                    }),
                    Err(err) => {
                        tracing::warn!(project = %project, deployment = %id.name, %err, "not a valid AssistantDeployment spec")
                    }
                }
            }
            _ => {}
        }
    }
    for (public_id, owners) in snapshot.claimed_twice() {
        tracing::warn!(%public_id, deployments = %owners.join(" and "), "a publicId is declared twice; neither deployment answers until one gives it up (MF-52)");
    }
    Ok(snapshot)
}

impl Snapshot {
    /// The anonymous deployment of `public_id`, when exactly one deployment of the organization
    /// declares that id. Two that claim it answer neither: no load order decides which project
    /// speaks on a public address (MF-52, T-3283).
    pub fn public(&self, public_id: &str) -> Option<&Deployment> {
        let mut claimants = self
            .deployments
            .iter()
            .filter(|d| d.spec.public_id == public_id);
        let only = claimants.next()?;
        (claimants.next().is_none() && only.spec.channel.is_anonymous()).then_some(only)
    }

    /// Every publicId more than one deployment declares, with `{project}/{name}` of each.
    pub fn claimed_twice(&self) -> Vec<(String, Vec<String>)> {
        let mut by_id: BTreeMap<&str, Vec<String>> = BTreeMap::new();
        for deployment in &self.deployments {
            by_id
                .entry(&deployment.spec.public_id)
                .or_default()
                .push(format!("{}/{}", deployment.project, deployment.name));
        }
        by_id
            .into_iter()
            .filter(|(_, owners)| owners.len() > 1)
            .map(|(id, owners)| (id.to_owned(), owners))
            .collect()
    }
}

/// Whether a source is due in the minute `now`: never read, or its schedule names it.
pub fn due(
    spec: &KnowledgeSourceSpec,
    last_crawl: Option<time::OffsetDateTime>,
    now: time::OffsetDateTime,
) -> bool {
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
    let outcome = match source.spec.source {
        SourceType::Website => crawl_site(
            pool,
            crawler,
            organization,
            &source.project,
            &source.name,
            &source.spec,
            &mut indexer,
        )
        .await
        .map(|report| {
            format!(
                "{} pages fetched, {} unchanged, {} documents",
                report.pages_fetched, report.pages_unchanged, report.documents_fetched
            )
        }),
        SourceType::Ckan => match &source.ckan_url {
            Some(url) => {
                let language = match source.spec.languages.as_slice() {
                    [one] => Some(one.as_str()),
                    _ => None,
                };
                sync_ckan(
                    pool,
                    crawler,
                    Catalogue {
                        project: &source.project,
                        site_id: site,
                        url,
                        max_datasets: source.spec.max_pages,
                        language,
                    },
                    &mut indexer,
                )
                .await
                .map(|report| {
                    format!(
                        "{} datasets, {} changed, {} removed",
                        report.datasets, report.changed, report.removed
                    )
                })
            }
            None => Err(Error::Crawl(format!(
                "the source names CkanInstance `{}`, which project {} does not declare",
                source.spec.ckan_instance_ref.as_deref().unwrap_or_default(),
                source.project
            ))),
        },
    };
    match outcome {
        Ok(summary) => {
            tracing::info!(
                project = %source.project, source = %source.name, %summary,
                passages = indexer.passages, failures = indexer.failures.len(), "read"
            );
            queue::finish(pool, job.id).await?;
        }
        Err(err) => {
            tracing::warn!(project = %source.project, source = %source.name, %err, "read failed, retried later");
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

#[cfg(test)]
mod tests {
    use super::*;

    fn deployment(project: &str, name: &str, public_id: &str, channel: &str) -> Deployment {
        let spec: AssistantDeploymentSpec = serde_json::from_value(serde_json::json!({
            "publicId": public_id,
            "channel": channel,
            "sources": ["web"],
        }))
        .expect("a spec");
        Deployment {
            project: project.into(),
            name: name.into(),
            spec,
        }
    }

    /// MF-52, T-3283: a publicId two deployments declare answers neither, in any load order;
    /// one alone answers when its channel is anonymous.
    #[test]
    fn a_public_id_claimed_twice_answers_neither() {
        let mut snapshot = Snapshot {
            deployments: vec![
                deployment("banskabystrica", "public", "city", "public"),
                deployment("praha", "lookalike", "city", "public"),
                deployment("praha", "own", "praha", "public"),
                deployment("praha", "staff", "staff", "internal"),
            ],
            ..Snapshot::default()
        };
        assert!(snapshot.public("city").is_none());
        snapshot.deployments.swap(0, 1);
        assert!(snapshot.public("city").is_none(), "no load order decides");
        assert_eq!(
            snapshot.public("praha").map(|d| d.name.as_str()),
            Some("own")
        );
        assert!(
            snapshot.public("staff").is_none(),
            "an internal deployment is not public"
        );
        assert!(snapshot.public("nobody").is_none());
        assert_eq!(
            snapshot.claimed_twice(),
            vec![(
                "city".to_owned(),
                vec![
                    "praha/lookalike".to_owned(),
                    "banskabystrica/public".to_owned()
                ]
            )]
        );
    }
}
