//! The knowledge assistant's website crawl core (T-3052).
//!
//! Discovers and fetches pages and linked documents breadth-first, enforcing SSRF egress guards,
//! robots.txt rules, path glob patterns, and byte caps. Results are recorded in the store's
//! tenant tables under project-scoped transactions and passed to the caller's [`Sink`](crate::crawl::Sink).

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use jc_core::kinds::assistant::{KnowledgeSourceSpec, PdfPolicyKind, SourceType, Visibility};
use reqwest::header::{HeaderMap, CONTENT_TYPE, ETAG, LAST_MODIFIED};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row};
use url::Url;

use crate::Error;

pub mod fetch;
pub mod links;
pub mod meta;
pub mod patterns;
pub mod queue;
pub mod robots;
pub mod sitemap;
pub mod wordpress;

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
        let client = fetch::client(resolver).map_err(Error::Crawl)?;
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
    /// Pages and documents deleted with their chunks: gone (404/410), or failed or unreached
    /// in [`GONE_AFTER_RUNS`] runs in a row (T-4749).
    pub removed: u32,
}

/// Runs in a row a page or document may fail to read, or a complete crawl may not reach it,
/// before it is deleted: one flaky day keeps it, a page gone for good leaves the index.
pub const GONE_AFTER_RUNS: i16 = 3;

/// An answer that says the resource is gone for good (RFC 9110 §15.5.5, §15.5.11).
fn is_gone(status: reqwest::StatusCode) -> bool {
    matches!(status.as_u16(), 404 | 410)
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

/// The addresses a crawl still has to read, in order, each queued once.
#[derive(Default)]
struct Frontier {
    queue: VecDeque<FrontierItem>,
    queued: HashSet<String>,
}

impl Frontier {
    fn push(&mut self, url: Url, parent_id: Option<i64>, depth: u8) {
        let url = canonicalise_url(url);
        if self.queued.insert(url.to_string()) {
            self.queue.push_back(FrontierItem {
                url,
                parent_id,
                depth,
            });
        }
    }
}

/// What one crawl has queued, seen and counted so far.
#[derive(Default)]
struct Walk {
    frontier: Frontier,
    seen_documents: HashSet<String>,
    /// A sitemap check: the pages it reads queue none of their links (T-4749).
    sitemap_only: bool,
    /// When the sitemap says each of its URLs last changed, by canonical URL (T-4749).
    lastmod: HashMap<String, String>,
    /// The pages this crawl visited, whatever they answered.
    reached: HashSet<String>,
    report: CrawlReport,
    /// The robots.txt of each other host a document of the site lives on, read once a crawl.
    foreign_robots: HashMap<String, robots::Robots>,
}

/// The first start URL and its host; a source that is not a website, or names no usable start
/// URL, is refused before anything is stored.
fn start_of(source: &str, spec: &KnowledgeSourceSpec) -> Result<(Url, String), Error> {
    if spec.source != SourceType::Website {
        return Err(Error::Crawl(format!(
            "source '{source}' has type {:?}, but crawl_site only supports website sources",
            spec.source
        )));
    }
    let Some(first) = spec.start_urls.first() else {
        return Err(Error::Crawl(format!(
            "website source '{source}' names no start URLs"
        )));
    };
    let start_url = Url::parse(first)
        .map_err(|err| Error::Crawl(format!("invalid start URL '{first}': {err}")))?;
    let host = start_url
        .host_str()
        .ok_or_else(|| Error::Crawl(format!("start URL '{first}' has no host")))?
        .to_owned();
    Ok((start_url, host))
}

/// The start address and host a site answers under (T-4749). `www.zilina.sk` and `www.praha.eu`
/// send every request to `zilina.sk` and `praha.eu`, and a crawl that kept the name it was given
/// read nothing: each answer "left the site". Only the start host's twin with or without
/// `www.`, on the same scheme and port, is adopted, so a source still reads only the site its
/// author named; any other redirect leaves the start as it was.
async fn settle_start(crawler: &Crawler, start_url: Url, host: String) -> (Url, String) {
    let outcome = fetch::fetch_url(
        &crawler.client,
        &crawler.policy,
        &host,
        &start_url,
        crawler.policy.max_page_bytes,
        None,
        None,
    )
    .await;
    match outcome {
        fetch::FetchOutcome::RedirectExternal { target_url }
            if target_url.scheme() == start_url.scheme()
                && target_url.port_or_known_default() == start_url.port_or_known_default()
                && target_url.host_str().is_some_and(|to| is_twin(&host, to)) =>
        {
            let twin = target_url.host_str().unwrap_or(&host).to_owned();
            tracing::info!(from = %host, to = %twin, "the site answers under its twin host");
            (target_url, twin)
        }
        _ => (start_url, host),
    }
}

/// Whether two hosts are one site's name with and without `www.`.
fn is_twin(a: &str, b: &str) -> bool {
    let a = a.to_ascii_lowercase();
    let b = b.to_ascii_lowercase();
    a.strip_prefix("www.") == Some(b.as_str()) || b.strip_prefix("www.") == Some(a.as_str())
}

/// `url` on `host` when it names the host's twin; any other address as it was.
fn rehost(mut url: Url, host: &str) -> Url {
    if url.host_str().is_some_and(|h| is_twin(h, host)) {
        let _ = url.set_host(Some(host));
    }
    url
}

/// A response's media type, without its parameters.
fn media_type(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.split(';').next().unwrap_or(s).trim())
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
    let names = Names {
        organization,
        project,
        source,
    };
    crawl(pool, crawler, names, spec, sink, false).await
}

/// The daily sitemap check of a website knowledge source (T-4749): only the pages its sitemap
/// lists as new to the store, or as changed since the crawl last read them, are read, with the
/// documents they link; no link is followed further and nothing unreached is aged. The site's
/// scheduled full crawl stays what finds what the sitemap misses.
pub async fn check_sitemap(
    pool: &PgPool,
    crawler: &Crawler,
    organization: &str,
    project: &str,
    source: &str,
    spec: &KnowledgeSourceSpec,
    sink: &mut impl Sink,
) -> Result<CrawlReport, Error> {
    let names = Names {
        organization,
        project,
        source,
    };
    crawl(pool, crawler, names, spec, sink, true).await
}

/// Whose source a crawl reads.
#[derive(Clone, Copy)]
struct Names<'a> {
    organization: &'a str,
    project: &'a str,
    source: &'a str,
}

async fn crawl(
    pool: &PgPool,
    crawler: &Crawler,
    names: Names<'_>,
    spec: &KnowledgeSourceSpec,
    sink: &mut impl Sink,
    sitemap_only: bool,
) -> Result<CrawlReport, Error> {
    let Names {
        organization,
        project,
        source,
    } = names;
    let (start_url, site_host) = start_of(source, spec)?;
    let (start_url, site_host) = settle_start(crawler, start_url, site_host).await;
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

    let mut listed = if spec.sitemap {
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
        sitemap::Listed::default()
    };

    let run = DocRun {
        pool,
        crawler,
        spec,
        site_host: &site_host,
        project,
        site_id,
        robots: &robots,
    };
    let mut walk = Walk {
        lastmod: std::mem::take(&mut listed.lastmod),
        sitemap_only,
        ..Walk::default()
    };
    if sitemap_only {
        let read = read_at(pool, project, site_id).await?;
        for u in listed.urls {
            if changed_since_read(&read, &walk.lastmod, u.as_str()) {
                walk.frontier.push(u, None, 1);
            }
        }
    } else {
        seed(run, &start_url, listed.urls, &mut walk, sink).await?;
    }

    let mut complete = !sitemap_only;
    while let Some(item) = walk.frontier.queue.pop_front() {
        if walk.report.pages_fetched + walk.report.pages_unchanged >= spec.max_pages {
            complete = false;
            break;
        }
        if item.url.host_str() != Some(&site_host) || item.depth > spec.max_depth {
            continue;
        }
        walk.reached.insert(item.url.to_string());
        visit(run, &item, &mut walk, sink).await?;
    }
    // Only a crawl that walked the whole site, under a robots.txt it could read, from a start
    // that answered, says what the site no longer links: a capped or blind run says nothing.
    let answered = walk.report.pages_fetched + walk.report.pages_unchanged > 0;
    if complete && answered && !matches!(robots, robots::Robots::DisallowAll) {
        walk.report.removed += age_unreached(pool, project, site_id, &walk).await?;
    }
    if !sitemap_only {
        update_site_last_crawl(pool, project, site_id).await?;
    }
    Ok(walk.report)
}

/// Whether a sitemap URL is new to the store, never read, or listed as changed after the crawl
/// last read it; one without a `<lastmod>` waits for the full crawl. Both dates are RFC 3339 in
/// UTC with whole seconds, so their text orders as their instants do.
fn changed_since_read(
    read: &HashMap<String, Option<String>>,
    lastmod: &HashMap<String, String>,
    url: &str,
) -> bool {
    match (read.get(url), lastmod.get(url)) {
        (None | Some(None), _) => true,
        (Some(Some(at)), Some(changed)) => changed > at,
        (Some(Some(_)), None) => false,
    }
}

/// When the crawl last read each page of the site, by URL; `None` for a page it never read.
async fn read_at(
    pool: &PgPool,
    project: &str,
    site_id: i64,
) -> Result<HashMap<String, Option<String>>, Error> {
    let mut tx = crate::project_scope(pool, project).await?;
    let rows: Vec<(String, Option<String>)> = sqlx::query_as(
        "SELECT url, to_char(fetched_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') \
         FROM pages WHERE site_id = $1",
    )
    .bind(site_id)
    .fetch_all(&mut *tx)
    .await?;
    tx.rollback().await?;
    Ok(rows.into_iter().collect())
}

/// Queues what a full crawl starts from: the start addresses, the WordPress site's own content
/// list, then the sitemap's pages.
async fn seed(
    run: DocRun<'_>,
    start_url: &Url,
    sitemap_urls: Vec<Url>,
    walk: &mut Walk,
    sink: &mut impl Sink,
) -> Result<(), Error> {
    let (crawler, spec, site_host) = (run.crawler, run.spec, run.site_host);
    // A WordPress site's own content list (T-4154): read before the sitemap, so with `maxPages`
    // reached it is the site's content that is kept, newest change first.
    let wordpress_urls = wordpress::fetch_wordpress_urls(
        &crawler.client,
        &crawler.policy,
        site_host,
        start_url,
        &spec.wordpress_types,
        spec.max_pages,
    )
    .await;
    for (i, url_str) in spec.start_urls.iter().enumerate() {
        let Ok(u) = Url::parse(url_str) else {
            continue;
        };
        // The first start address is where its redirect led; any other on the named host
        // moves to the host the site answers under.
        let u = if i == 0 {
            start_url.clone()
        } else {
            rehost(u, site_host)
        };
        let canon = canonicalise_url(u);
        // A start address that names a document is one document of the site (T-3293),
        // under the `pdf` policy and its bounds, not a page of its bytes.
        if links::is_document_url(&canon) {
            handle_document(run, None, &canon, walk, sink, false).await?;
        } else {
            walk.frontier.push(canon, None, 0);
        }
    }
    for u in wordpress_urls.into_iter().chain(sitemap_urls) {
        walk.frontier.push(u, None, 1);
    }
    Ok(())
}

/// Reads one queued page of the site and records what it answered.
async fn visit(
    run: DocRun<'_>,
    item: &FrontierItem,
    walk: &mut Walk,
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
    let included = patterns::is_included(item.url.path(), &spec.include, &spec.exclude);

    if !fetch::is_url_allowed(&item.url, crawler.policy.allow_plain_http) {
        walk.report.refused += 1;
        return run.skip(item, included).await;
    }
    if !robots.is_allowed(item.url.as_str()) {
        walk.report.disallowed += 1;
        return run.skip(item, included).await;
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
        site_host,
        &item.url,
        crawler.policy.max_page_bytes,
        stored_etag,
        stored_last_modified,
    )
    .await;

    match outcome {
        fetch::FetchOutcome::Refused => {
            walk.report.refused += 1;
            run.skip(item, included).await
        }
        fetch::FetchOutcome::TooLarge => {
            walk.report.too_large += 1;
            run.skip(item, included).await
        }
        fetch::FetchOutcome::Failed(ref why) => {
            walk.report.failed += 1;
            walk.report.removed += run.fail(item, included, why).await?;
            Ok(())
        }
        fetch::FetchOutcome::Status(status) if is_gone(status) => {
            walk.report.removed +=
                forget(pool, project, site_id, Table::Pages, item.url.as_str()).await?;
            Ok(())
        }
        fetch::FetchOutcome::Status(status) => {
            walk.report.failed += 1;
            walk.report.removed += run
                .fail(item, included, &format!("HTTP {}", status.as_u16()))
                .await?;
            Ok(())
        }
        fetch::FetchOutcome::RedirectExternal { target_url } => {
            if let Some(pid) = item.parent_id {
                walk.report.links +=
                    record_external_link(pool, project, pid, target_url.as_str()).await?;
            }
            run.skip(item, included).await
        }
        fetch::FetchOutcome::NotModified { .. } => {
            walk.report.pages_unchanged += 1;
            match &existing {
                Some(e) => revisit(run, item, e.id, walk, sink).await,
                None => Ok(()),
            }
        }
        fetch::FetchOutcome::Success {
            final_url,
            headers,
            body,
            ..
        } => {
            let page = Read {
                item,
                included,
                stored_hash,
                final_url: &final_url,
                headers: &headers,
            };
            read_page(run, page, &body, walk, sink).await
        }
    }
}

/// A page the server reports unchanged: its stored links are followed as if it had been read.
async fn revisit(
    run: DocRun<'_>,
    item: &FrontierItem,
    page_id: i64,
    walk: &mut Walk,
    sink: &mut impl Sink,
) -> Result<(), Error> {
    mark_page_not_modified(run.pool, run.project, page_id).await?;
    for (to_url, kind) in get_stored_links(run.pool, run.project, page_id).await? {
        let Ok(to_url) = Url::parse(&to_url) else {
            continue;
        };
        match kind.as_str() {
            "page" if item.depth < run.spec.max_depth && !walk.sitemap_only => {
                walk.frontier.push(to_url, Some(page_id), item.depth + 1);
            }
            "document" => {
                handle_document(run, Some(page_id), &to_url, walk, sink, false).await?;
            }
            _ => {}
        }
    }
    Ok(())
}

/// A page the server answered with content, and what the crawl knew of it before.
struct Read<'a> {
    item: &'a FrontierItem,
    included: bool,
    stored_hash: Option<&'a str>,
    final_url: &'a Url,
    headers: &'a HeaderMap,
}

/// Stores a page that answered, hands it to `sink` when new or changed, and queues its links.
async fn read_page(
    run: DocRun<'_>,
    page: Read<'_>,
    body: &[u8],
    walk: &mut Walk,
    sink: &mut impl Sink,
) -> Result<(), Error> {
    let Read {
        item,
        included,
        stored_hash,
        final_url,
        headers,
    } = page;
    // An address without a document's name that answers with a document's type
    // is read as that document, fetched again under the `pdf` bounds (T-3293).
    // ponytail: one extra fetch per such address; pass the body on if they turn common.
    if let Some(mime) = media_type(headers).filter(|m| links::is_document_type(m)) {
        let pdf = mime.eq_ignore_ascii_case("application/pdf");
        return handle_document(run, item.parent_id, final_url, walk, sink, pdf).await;
    }
    let body_str = String::from_utf8_lossy(body);
    let hash = hex::encode(Sha256::digest(body));
    let last_modified = headers.get(LAST_MODIFIED).and_then(|v| v.to_str().ok());
    let mut stated = meta::page(&body_str);
    // A page that states no date of change has changed when its sitemap, else its server,
    // says it did.
    stated.modified = stated
        .modified
        .or_else(|| walk.lastmod.get(item.url.as_str()).cloned())
        .or_else(|| last_modified.and_then(meta::instant));
    let language = stated.language.clone();
    let page_id = upsert_page_fetched(
        run.pool,
        run.project,
        run.site_id,
        PagePlace {
            url: final_url.as_str(),
            parent_id: item.parent_id,
            depth: item.depth as i16,
        },
        &stated,
        included,
        Validators {
            hash: &hash,
            etag: headers.get(ETAG).and_then(|v| v.to_str().ok()),
            last_modified,
        },
    )
    .await?;
    if stored_hash == Some(hash.as_str()) {
        walk.report.pages_unchanged += 1;
    } else {
        walk.report.pages_fetched += 1;
        if included {
            sink.page(page_id, final_url.as_str(), language.as_deref(), &body_str)
                .await;
        }
    }

    let extracted = links::extract_links(final_url, &body_str, run.site_host);
    walk.report.links += record_links(run.pool, run.project, page_id, &extracted).await?;

    for link in extracted {
        if !fetch::is_url_allowed(&link.url, run.crawler.policy.allow_plain_http) {
            walk.report.refused += 1;
            record_skipped_page(
                run.pool,
                run.project,
                run.site_id,
                link.url.as_str(),
                Some(page_id),
                (item.depth + 1) as i16,
                false,
            )
            .await?;
            continue;
        }
        match link.kind {
            links::LinkKind::Page if item.depth < run.spec.max_depth && !walk.sitemap_only => {
                walk.frontier.push(link.url, Some(page_id), item.depth + 1);
            }
            links::LinkKind::Document => {
                handle_document(run, Some(page_id), &link.url, walk, sink, false).await?;
            }
            links::LinkKind::Page | links::LinkKind::External => {}
        }
    }
    Ok(())
}

/// Records a redirect off the site as an external link of the page that linked it; 1 when new.
async fn record_external_link(
    pool: &PgPool,
    project: &str,
    from_page: i64,
    to_url: &str,
) -> Result<u32, Error> {
    let mut tx = crate::project_scope(pool, project).await?;
    let res = sqlx::query(
        "INSERT INTO links (from_page, project, to_url, kind) \
         VALUES ($1, $2, $3, 'external') \
         ON CONFLICT (from_page, to_url) DO NOTHING",
    )
    .bind(from_page)
    .bind(project)
    .bind(to_url)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(u32::from(res.rows_affected() > 0))
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

impl DocRun<'_> {
    /// Records a queued page that was not read.
    async fn skip(self, item: &FrontierItem, included: bool) -> Result<(), Error> {
        record_skipped_page(
            self.pool,
            self.project,
            self.site_id,
            item.url.as_str(),
            item.parent_id,
            item.depth as i16,
            included,
        )
        .await
        .map(drop)
    }

    /// Records a queued page whose read failed, and why; 1 when that was its last allowed run
    /// and it is deleted.
    async fn fail(self, item: &FrontierItem, included: bool, why: &str) -> Result<u32, Error> {
        record_failed_page(
            self.pool,
            self.project,
            self.site_id,
            item.url.as_str(),
            item.parent_id,
            item.depth as i16,
            included,
            why,
        )
        .await
    }
}

async fn handle_document(
    run: DocRun<'_>,
    page_id: Option<i64>,
    doc_url: &Url,
    walk: &mut Walk,
    sink: &mut impl Sink,
    answered_pdf: bool,
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

    if !walk.seen_documents.insert(url_str.to_owned()) {
        return Ok(());
    }
    let report = &mut walk.report;

    let off_domain = canon_doc_url.host_str() != Some(site_host);
    let is_pdf = answered_pdf || canon_doc_url.path().to_ascii_lowercase().ends_with(".pdf");

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

    // A document on another host answers to that host's robots.txt (T-4749): the site linking
    // it is not the one that decides whether a crawler may read it.
    let robots = match canon_doc_url.host_str() {
        Some(host) if host != site_host => {
            if !walk.foreign_robots.contains_key(host) {
                let robots_url = canon_doc_url
                    .join("/robots.txt")
                    .map(|u| u.to_string())
                    .unwrap_or_default();
                let (rules, _) =
                    robots::fetch_robots(&crawler.client, &crawler.policy, host, &robots_url).await;
                walk.foreign_robots.insert(host.to_owned(), rules);
            }
            walk.foreign_robots.get(host).unwrap_or(robots)
        }
        _ => robots,
    };
    if !robots.is_allowed(url_str) {
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
        fetch::FetchOutcome::Failed(ref why) => {
            report.failed += 1;
            let failed = Failed {
                page_id,
                url: url_str,
                off_domain,
                reason: why,
            };
            report.removed += record_failed_doc(pool, project, site_id, &failed).await?;
        }
        fetch::FetchOutcome::Status(status) if is_gone(status) => {
            report.removed += forget(pool, project, site_id, Table::Documents, url_str).await?;
        }
        fetch::FetchOutcome::Status(status) => {
            report.failed += 1;
            let reason = format!("HTTP {}", status.as_u16());
            let failed = Failed {
                page_id,
                url: url_str,
                off_domain,
                reason: &reason,
            };
            report.removed += record_failed_doc(pool, project, site_id, &failed).await?;
        }
        fetch::FetchOutcome::RedirectExternal { .. } => {
            report.failed += 1;
            let reason = "a redirect left the site";
            let failed = Failed {
                page_id,
                url: url_str,
                off_domain,
                reason,
            };
            report.removed += record_failed_doc(pool, project, site_id, &failed).await?;
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
            let mut stated = meta::pdf(&body);
            stated.modified = stated
                .modified
                .or_else(|| walk.lastmod.get(url_str).cloned())
                .or_else(|| last_modified.and_then(meta::instant));

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
                        stated: &stated,
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
                        stated: &stated,
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
             status = 'skipped', \
             failed_runs = 0 \
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

/// Why a read failed, as the index status shows it (T-4197): an HTTP status or the network
/// error, cut to the column's 300 characters; never what the page held.
fn reason(why: &str) -> String {
    why.chars().take(300).collect()
}

#[allow(clippy::too_many_arguments)]
async fn record_failed_page(
    pool: &PgPool,
    project: &str,
    site_id: i64,
    url: &str,
    parent_id: Option<i64>,
    depth: i16,
    included: bool,
    why: &str,
) -> Result<u32, Error> {
    let mut tx = crate::project_scope(pool, project).await?;
    let (id, runs): (i64, i16) = sqlx::query_as(
        "INSERT INTO pages (site_id, project, url, parent_id, depth, status, included, error, failed_runs) \
         VALUES ($1, $2, $3, $4, $5, 'failed', $6, $7, 1) \
         ON CONFLICT (site_id, url) DO UPDATE SET \
             parent_id = COALESCE(pages.parent_id, EXCLUDED.parent_id), \
             status = 'failed', \
             error = EXCLUDED.error, \
             failed_runs = pages.failed_runs + 1 \
         RETURNING id, failed_runs",
    )
    .bind(site_id)
    .bind(project)
    .bind(url)
    .bind(parent_id)
    .bind(depth)
    .bind(included)
    .bind(reason(why))
    .fetch_one(&mut *tx)
    .await?;
    let removed = drop_if_spent(&mut tx, Table::Pages, id, runs).await?;
    tx.commit().await?;
    Ok(removed)
}

/// The two tables of what a crawl read, each with the statements that remove from it.
#[derive(Clone, Copy)]
enum Table {
    Pages,
    Documents,
}

impl Table {
    fn delete_id(self) -> &'static str {
        match self {
            Table::Pages => "DELETE FROM pages WHERE id = $1",
            Table::Documents => "DELETE FROM documents WHERE id = $1",
        }
    }

    fn delete_url(self) -> &'static str {
        match self {
            Table::Pages => "DELETE FROM pages WHERE site_id = $1 AND url = $2",
            Table::Documents => "DELETE FROM documents WHERE site_id = $1 AND url = $2",
        }
    }

    fn age_unreached(self) -> &'static str {
        match self {
            Table::Pages => {
                "UPDATE pages SET failed_runs = failed_runs + 1 \
                 WHERE site_id = $1 AND NOT (url = ANY($2))"
            }
            Table::Documents => {
                "UPDATE documents SET failed_runs = failed_runs + 1 \
                 WHERE site_id = $1 AND NOT (url = ANY($2))"
            }
        }
    }

    fn delete_spent(self) -> &'static str {
        match self {
            Table::Pages => "DELETE FROM pages WHERE site_id = $1 AND failed_runs >= $2",
            Table::Documents => "DELETE FROM documents WHERE site_id = $1 AND failed_runs >= $2",
        }
    }
}

/// Deletes a page or document (its chunks and links go with it) whose failed runs reached
/// [`GONE_AFTER_RUNS`]; 1 when it did.
async fn drop_if_spent(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    table: Table,
    id: i64,
    runs: i16,
) -> Result<u32, Error> {
    if runs < GONE_AFTER_RUNS {
        return Ok(0);
    }
    let res = sqlx::query(table.delete_id())
        .bind(id)
        .execute(&mut **tx)
        .await?;
    Ok(u32::from(res.rows_affected() > 0))
}

/// Deletes the page or document of the site at `url`, which answered that it is gone; its
/// chunks and links go with it. 1 when there was one.
async fn forget(
    pool: &PgPool,
    project: &str,
    site_id: i64,
    table: Table,
    url: &str,
) -> Result<u32, Error> {
    let mut tx = crate::project_scope(pool, project).await?;
    let res = sqlx::query(table.delete_url())
        .bind(site_id)
        .bind(url)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(u32::from(res.rows_affected() > 0))
}

/// Counts one more missed run for every page and document of the site this crawl did not
/// reach, and deletes those that missed [`GONE_AFTER_RUNS`] in a row; how many it deleted.
async fn age_unreached(
    pool: &PgPool,
    project: &str,
    site_id: i64,
    walk: &Walk,
) -> Result<u32, Error> {
    let mut removed = 0;
    let mut tx = crate::project_scope(pool, project).await?;
    for (table, reached) in [
        (Table::Pages, &walk.reached),
        (Table::Documents, &walk.seen_documents),
    ] {
        let reached: Vec<&str> = reached.iter().map(String::as_str).collect();
        sqlx::query(table.age_unreached())
            .bind(site_id)
            .bind(&reached)
            .execute(&mut *tx)
            .await?;
        let res = sqlx::query(table.delete_spent())
            .bind(site_id)
            .bind(GONE_AFTER_RUNS)
            .execute(&mut *tx)
            .await?;
        removed += res.rows_affected() as u32;
    }
    tx.commit().await?;
    Ok(removed)
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
    stated: &meta::Meta,
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
        "INSERT INTO pages (site_id, project, url, parent_id, depth, status, content_hash, language, included, etag, last_modified, \
                            title, description, published, modified, fetched_at) \
         VALUES ($1, $2, $3, $4, $5, 'fetched', $6, $7, $8, $9, $10, \
                 $11, $12, $13::timestamptz, $14::timestamptz, now()) \
         ON CONFLICT (site_id, url) DO UPDATE SET \
             parent_id = COALESCE(pages.parent_id, EXCLUDED.parent_id), \
             title = EXCLUDED.title, \
             description = EXCLUDED.description, \
             published = EXCLUDED.published, \
             modified = EXCLUDED.modified, \
             depth = EXCLUDED.depth, \
             status = 'fetched', \
             content_hash = EXCLUDED.content_hash, \
             language = EXCLUDED.language, \
             included = EXCLUDED.included, \
             etag = EXCLUDED.etag, \
             last_modified = EXCLUDED.last_modified, \
             fetched_at = now(), \
             failed_runs = 0 \
         RETURNING id",
    )
    .bind(site_id)
    .bind(project)
    .bind(url)
    .bind(parent_id)
    .bind(depth)
    .bind(hash)
    .bind(stated.language.as_deref())
    .bind(included)
    .bind(etag)
    .bind(last_modified)
    .bind(stated.title.as_deref())
    .bind(stated.description.as_deref())
    .bind(stated.published.as_deref())
    .bind(stated.modified.as_deref())
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(id)
}

async fn mark_page_not_modified(pool: &PgPool, project: &str, page_id: i64) -> Result<(), Error> {
    let mut tx = crate::project_scope(pool, project).await?;
    sqlx::query(
        "UPDATE pages SET fetched_at = now(), status = 'fetched', failed_runs = 0 WHERE id = $1",
    )
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
             failed_runs = 0, \
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

/// A document whose read failed, and why.
struct Failed<'a> {
    page_id: Option<i64>,
    url: &'a str,
    off_domain: bool,
    reason: &'a str,
}

async fn record_failed_doc(
    pool: &PgPool,
    project: &str,
    site_id: i64,
    failed: &Failed<'_>,
) -> Result<u32, Error> {
    let mut tx = crate::project_scope(pool, project).await?;
    let (id, runs): (i64, i16) = sqlx::query_as(
        "INSERT INTO documents (site_id, project, page_id, url, off_domain, status, included, error, failed_runs) \
         VALUES ($1, $2, $3, $4, $5, 'failed', $6, $7, 1) \
         ON CONFLICT (site_id, url) DO UPDATE SET \
             page_id = COALESCE(documents.page_id, EXCLUDED.page_id), \
             off_domain = EXCLUDED.off_domain, \
             status = 'failed', \
             included = EXCLUDED.included, \
             error = EXCLUDED.error, \
             failed_runs = documents.failed_runs + 1 \
         RETURNING id, failed_runs",
    )
    .bind(site_id)
    .bind(project)
    .bind(failed.page_id)
    .bind(failed.url)
    .bind(failed.off_domain)
    .bind(true)
    .bind(reason(failed.reason))
    .fetch_one(&mut *tx)
    .await?;
    let removed = drop_if_spent(&mut tx, Table::Documents, id, runs).await?;
    tx.commit().await?;
    Ok(removed)
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
    stated: &'a meta::Meta,
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
        stated,
    } = facts;
    let Validators {
        hash,
        etag,
        last_modified,
    } = validators;
    let mut tx = crate::project_scope(pool, project).await?;
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO documents (site_id, project, page_id, url, mime, off_domain, bytes, content_hash, status, included, etag, last_modified, \
                                title, description, published, modified, fetched_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, 'fetched', $9, $10, $11, \
                 $12, $13, $14::timestamptz, $15::timestamptz, now()) \
         ON CONFLICT (site_id, url) DO UPDATE SET \
             page_id = COALESCE(documents.page_id, EXCLUDED.page_id), \
             title = EXCLUDED.title, \
             description = EXCLUDED.description, \
             published = EXCLUDED.published, \
             modified = EXCLUDED.modified, \
             mime = EXCLUDED.mime, \
             off_domain = EXCLUDED.off_domain, \
             bytes = EXCLUDED.bytes, \
             content_hash = EXCLUDED.content_hash, \
             status = 'fetched', \
             included = EXCLUDED.included, \
             etag = EXCLUDED.etag, \
             last_modified = EXCLUDED.last_modified, \
             fetched_at = now(), \
             failed_runs = 0 \
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
    .bind(stated.title.as_deref())
    .bind(stated.description.as_deref())
    .bind(stated.published.as_deref())
    .bind(stated.modified.as_deref())
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(id)
}

async fn mark_doc_not_modified(pool: &PgPool, project: &str, doc_id: i64) -> Result<(), Error> {
    let mut tx = crate::project_scope(pool, project).await?;
    sqlx::query("UPDATE documents SET fetched_at = now(), status = 'fetched', failed_runs = 0 WHERE id = $1")
        .bind(doc_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use jc_core::kinds::assistant::PdfPolicy;

    fn website(start_urls: &[&str]) -> KnowledgeSourceSpec {
        KnowledgeSourceSpec {
            source: SourceType::Website,
            categories: Vec::new(),
            start_urls: start_urls.iter().map(|u| (*u).to_owned()).collect(),
            ckan_instance_ref: None,
            context_spaces: Vec::new(),
            organizations: Vec::new(),
            upload: None,
            sitemap: true,
            wordpress_types: vec![],
            include: Vec::new(),
            exclude: Vec::new(),
            max_depth: 3,
            max_pages: 100,
            pdf: PdfPolicy::default(),
            off_domain_documents: false,
            schedule: None,
            languages: Vec::new(),
            visibility: Visibility::Public,
        }
    }

    fn refusal(source: &str, spec: &KnowledgeSourceSpec) -> String {
        match start_of(source, spec) {
            Err(Error::Crawl(why)) => why,
            other => panic!("expected a crawl refusal, got {other:?}"),
        }
    }

    #[test]
    fn the_first_start_url_names_the_site() {
        let spec = website(&["https://www.city.test:443/a#top", "https://other.test/"]);
        let (url, host) = start_of("city", &spec).expect("a website with a start URL");
        assert_eq!(url.as_str(), "https://www.city.test/a#top");
        assert_eq!(host, "www.city.test");
    }

    #[test]
    fn a_source_without_a_usable_start_url_is_refused_by_name() {
        assert!(refusal("city", &website(&[])).contains("'city' names no start URLs"));
        assert!(refusal("city", &website(&["not a url"])).contains("invalid start URL 'not a url'"));
        assert!(refusal("city", &website(&["mailto:a@b.test"])).contains("has no host"));
        let mut ckan = website(&["https://www.city.test/"]);
        ckan.source = SourceType::Ckan;
        assert!(refusal("city", &ckan).contains("only supports website sources"));
    }

    #[test]
    fn the_frontier_queues_an_address_once_whatever_its_fragment_or_default_port() {
        let mut frontier = Frontier::default();
        let url = |s: &str| Url::parse(s).expect("a test URL");
        frontier.push(url("https://www.city.test/a"), None, 0);
        frontier.push(url("https://www.city.test:443/a#section"), Some(7), 1);
        frontier.push(url("http://www.city.test:80/b"), Some(7), 2);
        frontier.push(url("http://www.city.test/b"), None, 0);
        let queued: Vec<_> = frontier
            .queue
            .iter()
            .map(|i| (i.url.as_str(), i.parent_id, i.depth))
            .collect();
        assert_eq!(
            queued,
            [
                ("https://www.city.test/a", None, 0),
                ("http://www.city.test/b", Some(7), 2)
            ]
        );
    }

    #[test]
    fn the_media_type_drops_its_parameters_and_a_missing_header_is_none() {
        let mut headers = HeaderMap::new();
        assert_eq!(media_type(&headers), None);
        headers.insert(
            CONTENT_TYPE,
            "application/PDF ; name=x".parse().expect("a header"),
        );
        assert_eq!(media_type(&headers), Some("application/PDF"));
        headers.insert(CONTENT_TYPE, "text/html".parse().expect("a header"));
        assert_eq!(media_type(&headers), Some("text/html"));
    }
}
