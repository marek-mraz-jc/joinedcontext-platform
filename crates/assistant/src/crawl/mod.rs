//! The knowledge assistant's website crawl core (T-3052).
//!
//! Discovers and fetches pages and linked documents breadth-first, enforcing SSRF egress guards,
//! robots.txt rules, path glob patterns, and byte caps. Results are recorded in the store's
//! tenant tables under project-scoped transactions and passed to the caller's [`Sink`].

use std::collections::{HashSet, VecDeque};
use std::sync::Arc;

use jc_core::kinds::assistant::{KnowledgeSourceSpec, PdfPolicyKind, SourceType, Visibility};
use reqwest::header::{CONTENT_TYPE, ETAG, LAST_MODIFIED};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row};
use url::Url;

use crate::Error;

pub mod fetch;
pub mod links;
pub mod patterns;
pub mod queue;
pub mod robots;
pub mod sitemap;

/// Maximum permitted HTML page size: 10 MiB (T-3052).
pub const MAX_PAGE_BYTES: u64 = 10 * 1024 * 1024;

/// Policy and size caps governing a crawl operation.
#[derive(Debug, Clone)]
pub struct CrawlPolicy {
    /// Whether plain `http` URLs are allowed (test fixtures only).
    pub allow_plain_http: bool,
    /// Maximum permitted page size in bytes. Default is [`MAX_PAGE_BYTES`].
    pub max_page_bytes: u64,
}

impl Default for CrawlPolicy {
    fn default() -> Self {
        Self {
            allow_plain_http: false,
            max_page_bytes: MAX_PAGE_BYTES,
        }
    }
}

/// Crawler executing website knowledge discovery.
#[derive(Debug, Clone)]
pub struct Crawler {
    /// Configured HTTP client.
    pub client: reqwest::Client,
    /// Policy and size caps.
    pub policy: CrawlPolicy,
}

impl Crawler {
    /// A crawler whose every name resolves through `resolver`. Production passes
    /// `agent_proxy::public_dns::PublicOnly` (see [`Crawler::public`]); a test passes a resolver
    /// for its fixture hosts.
    pub fn new<R: reqwest::dns::Resolve + 'static>(
        resolver: Arc<R>,
        policy: CrawlPolicy,
    ) -> Result<Self, Error> {
        let client = fetch::client(resolver)
            .map_err(|err| Error::Crawl(format!("the HTTP client could not be built: {err}")))?;
        Ok(Self { client, policy })
    }

    /// The production crawler: public addresses only, https only, the default caps.
    pub fn public() -> Result<Self, Error> {
        Self::new(
            Arc::new(agent_proxy::public_dns::PublicOnly),
            CrawlPolicy::default(),
        )
    }
}

/// Summary counts of a completed or partially completed crawl run.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CrawlReport {
    /// Number of pages fetched.
    pub pages_fetched: u32,
    /// Number of pages whose content was unchanged.
    pub pages_unchanged: u32,
    /// Number of documents fetched.
    pub documents_fetched: u32,
    /// Number of documents whose content was unchanged.
    pub documents_unchanged: u32,
    /// Number of documents excluded by policy or off-domain rules.
    pub documents_excluded: u32,
    /// Number of links recorded in the database.
    pub links: u32,
    /// Number of URLs disallowed by robots.txt.
    pub disallowed: u32,
    /// Number of URLs refused by SSRF checks.
    pub refused: u32,
    /// Number of responses exceeding byte caps.
    pub too_large: u32,
    /// Number of fetch failures (network, HTTP errors, etc.).
    pub failed: u32,
}

/// The consumer of crawled pages and documents (T-3052).
pub trait Sink {
    /// Invoked for an included page whose content is new or changed.
    fn page(
        &mut self,
        page_id: i64,
        url: &str,
        language: Option<&str>,
        html: &str,
    ) -> impl std::future::Future<Output = ()> + Send;

    /// Invoked for an included document whose content is new or changed.
    fn document(
        &mut self,
        document_id: i64,
        url: &str,
        mime: Option<&str>,
        bytes: &[u8],
    ) -> impl std::future::Future<Output = ()> + Send;
}

impl<T: Sink + ?Sized + Send> Sink for &mut T {
    fn page(
        &mut self,
        page_id: i64,
        url: &str,
        language: Option<&str>,
        html: &str,
    ) -> impl std::future::Future<Output = ()> + Send {
        (**self).page(page_id, url, language, html)
    }

    fn document(
        &mut self,
        document_id: i64,
        url: &str,
        mime: Option<&str>,
        bytes: &[u8],
    ) -> impl std::future::Future<Output = ()> + Send {
        (**self).document(document_id, url, mime, bytes)
    }
}

/// Canonicalises a URL by removing fragment identifiers and default ports (80 for http, 443 for https).
pub fn canonicalise_url(mut url: Url) -> Url {
    url.set_fragment(None);
    if url.scheme() == "http" && url.port() == Some(80) {
        let _ = url.set_port(None);
    }
    if url.scheme() == "https" && url.port() == Some(443) {
        let _ = url.set_port(None);
    }
    url
}

struct FrontierItem {
    url: Url,
    parent_id: Option<i64>,
    depth: u8,
}

struct ExistingRow {
    id: i64,
    content_hash: Option<String>,
    etag: Option<String>,
    last_modified: Option<String>,
}

/// Crawls a website knowledge source into the store, passing new or changed included items to `sink`.
pub async fn crawl_site(
    pool: &PgPool,
    crawler: &Crawler,
    organization: &str,
    project: &str,
    source: &str,
    spec: &KnowledgeSourceSpec,
    sink: &mut impl Sink,
) -> Result<CrawlReport, Error> {
    if spec.source != SourceType::Website {
        return Err(Error::Crawl(format!(
            "source '{source}' has type {:?}, but crawl_site only supports website sources",
            spec.source
        )));
    }
    if spec.start_urls.is_empty() {
        return Err(Error::Crawl(format!(
            "website source '{source}' names no start URLs"
        )));
    }

    let start_url = Url::parse(&spec.start_urls[0]).map_err(|err| {
        Error::Crawl(format!("invalid start URL '{}': {err}", spec.start_urls[0]))
    })?;
    let site_host = start_url
        .host_str()
        .ok_or_else(|| Error::Crawl(format!("start URL '{}' has no host", spec.start_urls[0])))?
        .to_owned();

    let site_id = upsert_site(pool, project, organization, source, spec.visibility).await?;

    let robots_url_str = match start_url.join("/robots.txt") {
        Ok(u) => u.to_string(),
        Err(_) => format!("{}://{}/robots.txt", start_url.scheme(), site_host),
    };
    let (robots, robots_sitemaps) = robots::fetch_robots(
        &crawler.client,
        &crawler.policy,
        &site_host,
        &robots_url_str,
    )
    .await;

    let sitemap_urls = if spec.sitemap {
        let mut initial_sitemaps = Vec::new();
        if let Ok(default_sm) = start_url.join("/sitemap.xml") {
            initial_sitemaps.push(default_sm.to_string());
        }
        initial_sitemaps.extend(robots_sitemaps);
        sitemap::fetch_sitemap_urls(
            &crawler.client,
            &crawler.policy,
            &site_host,
            &initial_sitemaps,
            spec.max_pages,
        )
        .await
    } else {
        Vec::new()
    };

    let mut frontier = VecDeque::new();
    let mut visited_urls = HashSet::new();
    let mut seen_documents = HashSet::new();
    let mut report = CrawlReport::default();

    for url_str in &spec.start_urls {
        if let Ok(u) = Url::parse(url_str) {
            let canon = canonicalise_url(u);
            let key = canon.to_string();
            if !visited_urls.contains(&key) {
                visited_urls.insert(key);
                frontier.push_back(FrontierItem {
                    url: canon,
                    parent_id: None,
                    depth: 0,
                });
            }
        }
    }

    for u in sitemap_urls {
        let canon = canonicalise_url(u);
        let key = canon.to_string();
        if !visited_urls.contains(&key) {
            visited_urls.insert(key);
            frontier.push_back(FrontierItem {
                url: canon,
                parent_id: None,
                depth: 1,
            });
        }
    }

    while let Some(item) = frontier.pop_front() {
        if report.pages_fetched + report.pages_unchanged >= spec.max_pages {
            break;
        }
        if item.url.host_str() != Some(&site_host) {
            continue;
        }
        if item.depth > spec.max_depth {
            continue;
        }

        let is_page_included = patterns::is_included(item.url.path(), &spec.include, &spec.exclude);

        if !fetch::is_url_allowed(&item.url, crawler.policy.allow_plain_http) {
            report.refused += 1;
            record_skipped_page(
                pool,
                project,
                site_id,
                item.url.as_str(),
                item.parent_id,
                item.depth as i16,
                is_page_included,
            )
            .await?;
            continue;
        }

        if !robots.is_allowed(item.url.as_str()) {
            report.disallowed += 1;
            record_skipped_page(
                pool,
                project,
                site_id,
                item.url.as_str(),
                item.parent_id,
                item.depth as i16,
                is_page_included,
            )
            .await?;
            continue;
        }

        let existing = get_existing_page(pool, project, site_id, item.url.as_str()).await?;
        let (stored_etag, stored_last_modified, stored_hash) = match &existing {
            Some(e) => (
                e.etag.as_deref(),
                e.last_modified.as_deref(),
                e.content_hash.as_deref(),
            ),
            None => (None, None, None),
        };

        let outcome = fetch::fetch_url(
            &crawler.client,
            &crawler.policy,
            &site_host,
            &item.url,
            crawler.policy.max_page_bytes,
            stored_etag,
            stored_last_modified,
        )
        .await;

        match outcome {
            fetch::FetchOutcome::Refused => {
                report.refused += 1;
                record_skipped_page(
                    pool,
                    project,
                    site_id,
                    item.url.as_str(),
                    item.parent_id,
                    item.depth as i16,
                    is_page_included,
                )
                .await?;
            }
            fetch::FetchOutcome::TooLarge => {
                report.too_large += 1;
                record_skipped_page(
                    pool,
                    project,
                    site_id,
                    item.url.as_str(),
                    item.parent_id,
                    item.depth as i16,
                    is_page_included,
                )
                .await?;
            }
            fetch::FetchOutcome::Failed(_) | fetch::FetchOutcome::Status(_) => {
                report.failed += 1;
                record_failed_page(
                    pool,
                    project,
                    site_id,
                    item.url.as_str(),
                    item.parent_id,
                    item.depth as i16,
                    is_page_included,
                )
                .await?;
            }
            fetch::FetchOutcome::RedirectExternal { target_url } => {
                if let Some(pid) = item.parent_id {
                    let mut tx = crate::project_scope(pool, project).await?;
                    let res = sqlx::query(
                        "INSERT INTO links (from_page, project, to_url, kind) \
                         VALUES ($1, $2, $3, 'external') \
                         ON CONFLICT (from_page, to_url) DO NOTHING",
                    )
                    .bind(pid)
                    .bind(project)
                    .bind(target_url.as_str())
                    .execute(&mut *tx)
                    .await?;
                    tx.commit().await?;
                    if res.rows_affected() > 0 {
                        report.links += 1;
                    }
                }
                record_skipped_page(
                    pool,
                    project,
                    site_id,
                    item.url.as_str(),
                    item.parent_id,
                    item.depth as i16,
                    is_page_included,
                )
                .await?;
            }
            fetch::FetchOutcome::NotModified { .. } => {
                report.pages_unchanged += 1;
                if let Some(e) = &existing {
                    mark_page_not_modified(pool, project, e.id).await?;
                    let stored_links = get_stored_links(pool, project, e.id).await?;
                    for (to_url_str, kind_str) in stored_links {
                        if let Ok(to_url) = Url::parse(&to_url_str) {
                            match kind_str.as_str() {
                                "page" => {
                                    if item.depth < spec.max_depth {
                                        let canon = canonicalise_url(to_url);
                                        let key = canon.to_string();
                                        if !visited_urls.contains(&key) {
                                            visited_urls.insert(key);
                                            frontier.push_back(FrontierItem {
                                                url: canon,
                                                parent_id: Some(e.id),
                                                depth: item.depth + 1,
                                            });
                                        }
                                    }
                                }
                                "document" => {
                                    handle_document(
                                        DocRun {
                                            pool,
                                            crawler,
                                            spec,
                                            site_host: &site_host,
                                            project,
                                            site_id,
                                            robots: &robots,
                                        },
                                        Some(e.id),
                                        &to_url,
                                        &mut seen_documents,
                                        &mut report,
                                        sink,
                                    )
                                    .await?;
                                }
                                _ => {}
                            }
                        }
                    }
                }
            }
            fetch::FetchOutcome::Success {
                final_url,
                headers,
                body,
                ..
            } => {
                let body_str = String::from_utf8_lossy(&body);
                let hash = hex::encode(Sha256::digest(&body));
                let language = links::extract_language(&body_str);
                let etag = headers.get(ETAG).and_then(|v| v.to_str().ok());
                let last_modified = headers.get(LAST_MODIFIED).and_then(|v| v.to_str().ok());

                let is_unchanged = stored_hash == Some(&hash);
                let page_id = if is_unchanged {
                    report.pages_unchanged += 1;
                    upsert_page_fetched(
                        pool,
                        project,
                        site_id,
                        PagePlace {
                            url: final_url.as_str(),
                            parent_id: item.parent_id,
                            depth: item.depth as i16,
                        },
                        language.as_deref(),
                        is_page_included,
                        Validators {
                            hash: &hash,
                            etag,
                            last_modified,
                        },
                    )
                    .await?
                } else {
                    report.pages_fetched += 1;
                    let pid = upsert_page_fetched(
                        pool,
                        project,
                        site_id,
                        PagePlace {
                            url: final_url.as_str(),
                            parent_id: item.parent_id,
                            depth: item.depth as i16,
                        },
                        language.as_deref(),
                        is_page_included,
                        Validators {
                            hash: &hash,
                            etag,
                            last_modified,
                        },
                    )
                    .await?;
                    if is_page_included {
                        sink.page(pid, final_url.as_str(), language.as_deref(), &body_str)
                            .await;
                    }
                    pid
                };

                let extracted = links::extract_links(&final_url, &body_str, &site_host);
                let inserted_links = record_links(pool, project, page_id, &extracted).await?;
                report.links += inserted_links;

                for link in extracted {
                    if !fetch::is_url_allowed(&link.url, crawler.policy.allow_plain_http) {
                        report.refused += 1;
                        record_skipped_page(
                            pool,
                            project,
                            site_id,
                            link.url.as_str(),
                            Some(page_id),
                            (item.depth + 1) as i16,
                            false,
                        )
                        .await?;
                        continue;
                    }

                    match link.kind {
                        links::LinkKind::Page => {
                            if item.depth < spec.max_depth {
                                let canon = canonicalise_url(link.url);
                                let key = canon.to_string();
                                if !visited_urls.contains(&key) {
                                    visited_urls.insert(key);
                                    frontier.push_back(FrontierItem {
                                        url: canon,
                                        parent_id: Some(page_id),
                                        depth: item.depth + 1,
                                    });
                                }
                            }
                        }
                        links::LinkKind::Document => {
                            handle_document(
                                DocRun {
                                    pool,
                                    crawler,
                                    spec,
                                    site_host: &site_host,
                                    project,
                                    site_id,
                                    robots: &robots,
                                },
                                Some(page_id),
                                &link.url,
                                &mut seen_documents,
                                &mut report,
                                sink,
                            )
                            .await?;
                        }
                        links::LinkKind::External => {}
                    }
                }
            }
        }
    }

    update_site_last_crawl(pool, project, site_id).await?;
    Ok(report)
}

/// What every document of one crawl shares: where it is stored and what governs it.
#[derive(Clone, Copy)]
struct DocRun<'a> {
    pool: &'a PgPool,
    crawler: &'a Crawler,
    spec: &'a KnowledgeSourceSpec,
    site_host: &'a str,
    project: &'a str,
    site_id: i64,
    robots: &'a robots::Robots,
}

async fn handle_document(
    run: DocRun<'_>,
    page_id: Option<i64>,
    doc_url: &Url,
    seen_documents: &mut HashSet<String>,
    report: &mut CrawlReport,
    sink: &mut impl Sink,
) -> Result<(), Error> {
    let DocRun {
        pool,
        crawler,
        spec,
        site_host,
        project,
        site_id,
        robots,
    } = run;
    let canon_doc_url = canonicalise_url(doc_url.clone());
    let url_str = canon_doc_url.as_str();

    if !seen_documents.insert(url_str.to_owned()) {
        return Ok(());
    }

    let off_domain = canon_doc_url.host_str() != Some(site_host);
    let is_pdf = canon_doc_url.path().to_ascii_lowercase().ends_with(".pdf");

    let mut included = !off_domain || spec.off_domain_documents;
    if is_pdf && spec.pdf.policy == PdfPolicyKind::Exclude {
        included = false;
    }

    if !included {
        report.documents_excluded += 1;
        record_skipped_doc(pool, project, site_id, page_id, url_str, off_domain, false).await?;
        return Ok(());
    }

    if !fetch::is_url_allowed(&canon_doc_url, crawler.policy.allow_plain_http) {
        report.refused += 1;
        record_skipped_doc(pool, project, site_id, page_id, url_str, off_domain, true).await?;
        return Ok(());
    }

    if canon_doc_url.host_str() == Some(site_host) && !robots.is_allowed(url_str) {
        report.disallowed += 1;
        record_skipped_doc(pool, project, site_id, page_id, url_str, off_domain, true).await?;
        return Ok(());
    }

    let existing = get_existing_doc(pool, project, site_id, url_str).await?;
    let (stored_etag, stored_last_modified, stored_hash) = match &existing {
        Some(e) => (
            e.etag.as_deref(),
            e.last_modified.as_deref(),
            e.content_hash.as_deref(),
        ),
        None => (None, None, None),
    };

    let doc_host = canon_doc_url.host_str().unwrap_or(site_host);
    let outcome = fetch::fetch_url(
        &crawler.client,
        &crawler.policy,
        doc_host,
        &canon_doc_url,
        spec.pdf.max_bytes,
        stored_etag,
        stored_last_modified,
    )
    .await;

    match outcome {
        fetch::FetchOutcome::Refused => {
            report.refused += 1;
            record_skipped_doc(pool, project, site_id, page_id, url_str, off_domain, true).await?;
        }
        fetch::FetchOutcome::TooLarge => {
            report.too_large += 1;
            record_skipped_doc(pool, project, site_id, page_id, url_str, off_domain, true).await?;
        }
        fetch::FetchOutcome::Failed(_) | fetch::FetchOutcome::Status(_) => {
            report.failed += 1;
            record_failed_doc(pool, project, site_id, page_id, url_str, off_domain, true).await?;
        }
        fetch::FetchOutcome::RedirectExternal { .. } => {
            report.failed += 1;
            record_failed_doc(pool, project, site_id, page_id, url_str, off_domain, true).await?;
        }
        fetch::FetchOutcome::NotModified { .. } => {
            report.documents_unchanged += 1;
            if let Some(e) = &existing {
                mark_doc_not_modified(pool, project, e.id).await?;
            }
        }
        fetch::FetchOutcome::Success { headers, body, .. } => {
            let hash = hex::encode(Sha256::digest(&body));
            let mime = headers
                .get(CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .map(|s| s.split(';').next().unwrap_or(s).trim());
            let etag = headers.get(ETAG).and_then(|v| v.to_str().ok());
            let last_modified = headers.get(LAST_MODIFIED).and_then(|v| v.to_str().ok());
            let bytes_count = body.len() as i64;

            let is_unchanged = stored_hash == Some(&hash);
            if is_unchanged {
                report.documents_unchanged += 1;
                upsert_doc_fetched(
                    pool,
                    project,
                    site_id,
                    DocFacts {
                        page_id,
                        url: url_str,
                        mime,
                        off_domain,
                        bytes: bytes_count,
                        included: true,
                    },
                    Validators {
                        hash: &hash,
                        etag,
                        last_modified,
                    },
                )
                .await?;
            } else {
                report.documents_fetched += 1;
                let doc_id = upsert_doc_fetched(
                    pool,
                    project,
                    site_id,
                    DocFacts {
                        page_id,
                        url: url_str,
                        mime,
                        off_domain,
                        bytes: bytes_count,
                        included: true,
                    },
                    Validators {
                        hash: &hash,
                        etag,
                        last_modified,
                    },
                )
                .await?;
                sink.document(doc_id, url_str, mime, &body).await;
            }
        }
    }

    Ok(())
}

/// The site row of a source, created on its first crawl; its id is what [`crate::extract::Indexer`]
/// stores chunks under.
pub async fn upsert_site(
    pool: &PgPool,
    project: &str,
    organization: &str,
    source: &str,
    visibility: Visibility,
) -> Result<i64, Error> {
    let vis_str = match visibility {
        Visibility::Public => "public",
        Visibility::Internal => "internal",
    };
    let mut tx = crate::project_scope(pool, project).await?;
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO sites (organization, project, source, visibility) \
         VALUES ($1, $2, $3, $4) \
         ON CONFLICT (project, source) DO UPDATE SET visibility = EXCLUDED.visibility \
         RETURNING id",
    )
    .bind(organization)
    .bind(project)
    .bind(source)
    .bind(vis_str)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(id)
}

async fn update_site_last_crawl(pool: &PgPool, project: &str, site_id: i64) -> Result<(), Error> {
    let mut tx = crate::project_scope(pool, project).await?;
    sqlx::query("UPDATE sites SET last_crawl = now() WHERE id = $1")
        .bind(site_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

async fn get_existing_page(
    pool: &PgPool,
    project: &str,
    site_id: i64,
    url: &str,
) -> Result<Option<ExistingRow>, Error> {
    let mut tx = crate::project_scope(pool, project).await?;
    let row = sqlx::query(
        "SELECT id, content_hash, etag, last_modified FROM pages WHERE site_id = $1 AND url = $2",
    )
    .bind(site_id)
    .bind(url)
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;
    match row {
        Some(r) => Ok(Some(ExistingRow {
            id: r.try_get("id")?,
            content_hash: r.try_get("content_hash")?,
            etag: r.try_get("etag")?,
            last_modified: r.try_get("last_modified")?,
        })),
        None => Ok(None),
    }
}

async fn get_existing_doc(
    pool: &PgPool,
    project: &str,
    site_id: i64,
    url: &str,
) -> Result<Option<ExistingRow>, Error> {
    let mut tx = crate::project_scope(pool, project).await?;
    let row = sqlx::query(
        "SELECT id, content_hash, etag, last_modified FROM documents WHERE site_id = $1 AND url = $2",
    )
    .bind(site_id)
    .bind(url)
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;
    match row {
        Some(r) => Ok(Some(ExistingRow {
            id: r.try_get("id")?,
            content_hash: r.try_get("content_hash")?,
            etag: r.try_get("etag")?,
            last_modified: r.try_get("last_modified")?,
        })),
        None => Ok(None),
    }
}

async fn record_skipped_page(
    pool: &PgPool,
    project: &str,
    site_id: i64,
    url: &str,
    parent_id: Option<i64>,
    depth: i16,
    included: bool,
) -> Result<i64, Error> {
    let mut tx = crate::project_scope(pool, project).await?;
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO pages (site_id, project, url, parent_id, depth, status, included) \
         VALUES ($1, $2, $3, $4, $5, 'skipped', $6) \
         ON CONFLICT (site_id, url) DO UPDATE SET \
             parent_id = COALESCE(pages.parent_id, EXCLUDED.parent_id), \
             status = 'skipped' \
         RETURNING id",
    )
    .bind(site_id)
    .bind(project)
    .bind(url)
    .bind(parent_id)
    .bind(depth)
    .bind(included)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(id)
}

async fn record_failed_page(
    pool: &PgPool,
    project: &str,
    site_id: i64,
    url: &str,
    parent_id: Option<i64>,
    depth: i16,
    included: bool,
) -> Result<i64, Error> {
    let mut tx = crate::project_scope(pool, project).await?;
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO pages (site_id, project, url, parent_id, depth, status, included) \
         VALUES ($1, $2, $3, $4, $5, 'failed', $6) \
         ON CONFLICT (site_id, url) DO UPDATE SET \
             parent_id = COALESCE(pages.parent_id, EXCLUDED.parent_id), \
             status = 'failed' \
         RETURNING id",
    )
    .bind(site_id)
    .bind(project)
    .bind(url)
    .bind(parent_id)
    .bind(depth)
    .bind(included)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(id)
}

/// What tells a later crawl whether a resource changed: its SHA-256 and the server's validators.
#[derive(Clone, Copy)]
struct Validators<'a> {
    hash: &'a str,
    etag: Option<&'a str>,
    last_modified: Option<&'a str>,
}

/// Where one page sits in its site's tree.
#[derive(Clone, Copy)]
struct PagePlace<'a> {
    url: &'a str,
    parent_id: Option<i64>,
    depth: i16,
}

async fn upsert_page_fetched(
    pool: &PgPool,
    project: &str,
    site_id: i64,
    place: PagePlace<'_>,
    language: Option<&str>,
    included: bool,
    validators: Validators<'_>,
) -> Result<i64, Error> {
    let PagePlace {
        url,
        parent_id,
        depth,
    } = place;
    let Validators {
        hash,
        etag,
        last_modified,
    } = validators;
    let mut tx = crate::project_scope(pool, project).await?;
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO pages (site_id, project, url, parent_id, depth, status, content_hash, language, included, etag, last_modified, fetched_at) \
         VALUES ($1, $2, $3, $4, $5, 'fetched', $6, $7, $8, $9, $10, now()) \
         ON CONFLICT (site_id, url) DO UPDATE SET \
             parent_id = COALESCE(pages.parent_id, EXCLUDED.parent_id), \
             depth = EXCLUDED.depth, \
             status = 'fetched', \
             content_hash = EXCLUDED.content_hash, \
             language = EXCLUDED.language, \
             included = EXCLUDED.included, \
             etag = EXCLUDED.etag, \
             last_modified = EXCLUDED.last_modified, \
             fetched_at = now() \
         RETURNING id",
    )
    .bind(site_id)
    .bind(project)
    .bind(url)
    .bind(parent_id)
    .bind(depth)
    .bind(hash)
    .bind(language)
    .bind(included)
    .bind(etag)
    .bind(last_modified)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(id)
}

async fn mark_page_not_modified(pool: &PgPool, project: &str, page_id: i64) -> Result<(), Error> {
    let mut tx = crate::project_scope(pool, project).await?;
    sqlx::query("UPDATE pages SET fetched_at = now(), status = 'fetched' WHERE id = $1")
        .bind(page_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

async fn record_links(
    pool: &PgPool,
    project: &str,
    page_id: i64,
    extracted_links: &[links::ExtractedLink],
) -> Result<u32, Error> {
    if extracted_links.is_empty() {
        return Ok(0);
    }
    let mut tx = crate::project_scope(pool, project).await?;
    let mut inserted = 0;
    for link in extracted_links {
        let res = sqlx::query(
            "INSERT INTO links (from_page, project, to_url, kind) \
             VALUES ($1, $2, $3, $4) \
             ON CONFLICT (from_page, to_url) DO NOTHING",
        )
        .bind(page_id)
        .bind(project)
        .bind(link.url.as_str())
        .bind(link.kind.as_str())
        .execute(&mut *tx)
        .await?;
        inserted += res.rows_affected() as u32;
    }
    tx.commit().await?;
    Ok(inserted)
}

async fn get_stored_links(
    pool: &PgPool,
    project: &str,
    page_id: i64,
) -> Result<Vec<(String, String)>, Error> {
    let mut tx = crate::project_scope(pool, project).await?;
    let rows = sqlx::query("SELECT to_url, kind FROM links WHERE from_page = $1")
        .bind(page_id)
        .fetch_all(&mut *tx)
        .await?;
    tx.commit().await?;
    let mut links = Vec::with_capacity(rows.len());
    for r in rows {
        links.push((r.try_get("to_url")?, r.try_get("kind")?));
    }
    Ok(links)
}

async fn record_skipped_doc(
    pool: &PgPool,
    project: &str,
    site_id: i64,
    page_id: Option<i64>,
    url: &str,
    off_domain: bool,
    included: bool,
) -> Result<i64, Error> {
    let mut tx = crate::project_scope(pool, project).await?;
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO documents (site_id, project, page_id, url, off_domain, status, included) \
         VALUES ($1, $2, $3, $4, $5, 'skipped', $6) \
         ON CONFLICT (site_id, url) DO UPDATE SET \
             page_id = COALESCE(documents.page_id, EXCLUDED.page_id), \
             off_domain = EXCLUDED.off_domain, \
             status = 'skipped', \
             included = EXCLUDED.included \
         RETURNING id",
    )
    .bind(site_id)
    .bind(project)
    .bind(page_id)
    .bind(url)
    .bind(off_domain)
    .bind(included)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(id)
}

async fn record_failed_doc(
    pool: &PgPool,
    project: &str,
    site_id: i64,
    page_id: Option<i64>,
    url: &str,
    off_domain: bool,
    included: bool,
) -> Result<i64, Error> {
    let mut tx = crate::project_scope(pool, project).await?;
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO documents (site_id, project, page_id, url, off_domain, status, included) \
         VALUES ($1, $2, $3, $4, $5, 'failed', $6) \
         ON CONFLICT (site_id, url) DO UPDATE SET \
             page_id = COALESCE(documents.page_id, EXCLUDED.page_id), \
             off_domain = EXCLUDED.off_domain, \
             status = 'failed', \
             included = EXCLUDED.included \
         RETURNING id",
    )
    .bind(site_id)
    .bind(project)
    .bind(page_id)
    .bind(url)
    .bind(off_domain)
    .bind(included)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(id)
}

/// What a fetched document is, beside its validators.
#[derive(Clone, Copy)]
struct DocFacts<'a> {
    page_id: Option<i64>,
    url: &'a str,
    mime: Option<&'a str>,
    off_domain: bool,
    bytes: i64,
    included: bool,
}

async fn upsert_doc_fetched(
    pool: &PgPool,
    project: &str,
    site_id: i64,
    facts: DocFacts<'_>,
    validators: Validators<'_>,
) -> Result<i64, Error> {
    let DocFacts {
        page_id,
        url,
        mime,
        off_domain,
        bytes,
        included,
    } = facts;
    let Validators {
        hash,
        etag,
        last_modified,
    } = validators;
    let mut tx = crate::project_scope(pool, project).await?;
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO documents (site_id, project, page_id, url, mime, off_domain, bytes, content_hash, status, included, etag, last_modified, fetched_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, 'fetched', $9, $10, $11, now()) \
         ON CONFLICT (site_id, url) DO UPDATE SET \
             page_id = COALESCE(documents.page_id, EXCLUDED.page_id), \
             mime = EXCLUDED.mime, \
             off_domain = EXCLUDED.off_domain, \
             bytes = EXCLUDED.bytes, \
             content_hash = EXCLUDED.content_hash, \
             status = 'fetched', \
             included = EXCLUDED.included, \
             etag = EXCLUDED.etag, \
             last_modified = EXCLUDED.last_modified, \
             fetched_at = now() \
         RETURNING id",
    )
    .bind(site_id)
    .bind(project)
    .bind(page_id)
    .bind(url)
    .bind(mime)
    .bind(off_domain)
    .bind(bytes)
    .bind(hash)
    .bind(included)
    .bind(etag)
    .bind(last_modified)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(id)
}

async fn mark_doc_not_modified(pool: &PgPool, project: &str, doc_id: i64) -> Result<(), Error> {
    let mut tx = crate::project_scope(pool, project).await?;
    sqlx::query("UPDATE documents SET fetched_at = now(), status = 'fetched' WHERE id = $1")
        .bind(doc_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}
