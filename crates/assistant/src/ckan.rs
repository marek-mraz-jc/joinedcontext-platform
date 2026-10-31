//! A `ckan` knowledge source (T-3054, ADR-N-040): the public datasets of a catalogue, read with
//! the Action API, each dataset's description and resource list indexed as one page whose
//! citation is the dataset's page on the catalogue.
//!
//! Every run lists the catalogue's datasets anonymously, so only public ones come back (and a
//! dataset marked private is skipped all the same); a dataset whose `metadata_modified` changed
//! is indexed again, one the catalogue no longer lists is removed with its passages. A listing
//! that fails part way removes nothing. Rows are data, not knowledge: CKAN holds none (T-5060),
//! and the assistant reaches them through its MCP connectors, never through this index.

use crate::html_escape as escape;
use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use regex::Regex;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use url::Url;

use crate::crawl::fetch::{fetch_url, FetchOutcome};
use crate::crawl::{Crawler, Sink, MAX_PAGE_BYTES};
use crate::Error;

/// Datasets per listing request.
pub const ROWS: u64 = 100;

/// What one run did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CkanReport {
    /// Public datasets the catalogue listed (up to the source's `maxPages`).
    pub datasets: usize,
    /// Of those, the ones new or changed since the last run, indexed again.
    pub changed: usize,
    /// Datasets indexed before that the catalogue no longer lists, removed.
    pub removed: usize,
}

#[derive(Deserialize)]
struct Envelope<T> {
    #[serde(default)]
    success: bool,
    result: Option<T>,
}

#[derive(Deserialize)]
struct SearchPage {
    count: u64,
    #[serde(default)]
    results: Vec<Dataset>,
}

#[derive(Deserialize)]
struct Dataset {
    name: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    notes: Option<String>,
    #[serde(default)]
    private: bool,
    #[serde(default)]
    metadata_modified: Option<String>,
    #[serde(default)]
    organization: Option<Organization>,
    #[serde(default)]
    tags: Vec<Tag>,
    #[serde(default)]
    resources: Vec<Resource>,
}

#[derive(Deserialize)]
struct Organization {
    #[serde(default)]
    title: Option<String>,
}

#[derive(Deserialize)]
struct Tag {
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    name: Option<String>,
}

#[derive(Deserialize)]
struct Resource {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    format: Option<String>,
    #[serde(default)]
    url: Option<String>,
}

/// A CKAN dataset name: 2 to 100 of `a-z`, `0-9`, `-` and `_`. Anything else is not put into a
/// citation URL.
fn is_dataset_name(name: &str) -> bool {
    (2..=100).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

fn present(value: &Option<String>) -> Option<&str> {
    value.as_deref().map(str::trim).filter(|v| !v.is_empty())
}

/// The page a dataset is indexed as: its title, description, publisher, keywords and the name,
/// format and description of each resource, with the address of one an Endpoint serves, so a
/// citation of that Endpoint finds this page (T-4366). HTML, so it goes through the same extraction as a
/// crawled page; every value is escaped.
fn dataset_html(dataset: &Dataset) -> String {
    let mut html = String::from("<html><body>");
    let title = present(&dataset.title).unwrap_or(&dataset.name);
    html.push_str(&format!("<h1>{}</h1>", escape(title)));
    if let Some(notes) = present(&dataset.notes) {
        for paragraph in notes.split("\n\n").map(str::trim).filter(|p| !p.is_empty()) {
            html.push_str(&format!("<p>{}</p>", escape(paragraph)));
        }
    }
    if let Some(publisher) = dataset
        .organization
        .as_ref()
        .and_then(|o| present(&o.title))
    {
        html.push_str(&format!("<p>Publisher: {}</p>", escape(publisher)));
    }
    let tags: Vec<&str> = dataset
        .tags
        .iter()
        .filter_map(|t| present(&t.display_name).or(present(&t.name)))
        .collect();
    if !tags.is_empty() {
        html.push_str(&format!("<p>Keywords: {}</p>", escape(&tags.join(", "))));
    }
    if !dataset.resources.is_empty() {
        html.push_str("<h2>Resources</h2><ul>");
        for resource in &dataset.resources {
            let mut item = escape(present(&resource.name).unwrap_or("Unnamed resource"));
            if let Some(format) = present(&resource.format) {
                item.push_str(&format!(" ({})", escape(format)));
            }
            match present(&resource.url).filter(|u| u.contains("/api/endpoint/")) {
                // An interface of the Endpoint: its addresses, the typed example among them
                // (T-4625), but not its how-to, which is the same on every dataset page and is
                // the catalogue's own page instead (T-4747).
                Some(url) => {
                    let mut addresses = vec![url];
                    for found in present(&resource.description)
                        .into_iter()
                        .flat_map(|d| ADDRESS.find_iter(d))
                    {
                        // A sentence's own stop is not part of the address.
                        let found = found.as_str().trim_end_matches(['.', ',', ';', ':']);
                        if !addresses.contains(&found) {
                            addresses.push(found);
                        }
                    }
                    for address in addresses {
                        item.push_str(&format!(" ({})", escape(address)));
                    }
                }
                None => {
                    if let Some(description) = present(&resource.description) {
                        item.push_str(&format!(": {}", escape(description)));
                    }
                }
            }
            html.push_str(&format!("<li>{item}</li>"));
        }
        html.push_str("</ul>");
    }
    html.push_str("</body></html>");
    html
}

/// An Endpoint's address in a resource's description.
static ADDRESS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"https?://[^\s'"<>()]+/api/endpoint/[^\s'"<>()]+"#).expect("a pattern")
});

/// One Endpoint's slug and one type in an address, which the how-to page names generically.
static SLUG: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"/api/endpoint/[a-z0-9-]+").expect("a pattern"));
static TYPE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"type=[A-Za-z0-9_]+").expect("a pattern"));

/// How to reach the catalogue's data, from the interface descriptions of `dataset`: the text the
/// catalogue gives every dataset the same, indexed once at the catalogue's own address, so a
/// question about connecting cites the catalogue, never one dataset (T-4747). Its addresses name
/// `{endpoint}` and `{type}`, so no Endpoint's citation finds this page. `None` when the
/// dataset has no interface with a description.
fn connect_html(dataset: &Dataset) -> Option<String> {
    let mut body = String::new();
    for resource in &dataset.resources {
        let Some(description) = present(&resource.description) else {
            continue;
        };
        if !present(&resource.url).is_some_and(|u| u.contains("/api/endpoint/")) {
            continue;
        }
        let generic = SLUG.replace_all(description, "/api/endpoint/{endpoint}");
        let generic = TYPE.replace_all(&generic, "type={type}");
        let mut heading = escape(present(&resource.name).unwrap_or("Interface"));
        if let Some(format) = present(&resource.format) {
            heading.push_str(&format!(" ({})", escape(format)));
        }
        body.push_str(&format!("<h2>{heading}</h2>"));
        for paragraph in generic
            .split("\n\n")
            .map(str::trim)
            .filter(|p| !p.is_empty())
        {
            body.push_str(&format!("<p>{}</p>", escape(paragraph)));
        }
    }
    (!body.is_empty()).then(|| {
        format!("<html><body><h1>Connecting to the catalogue's data</h1>{body}</body></html>")
    })
}

/// The catalogue's base URL with the trailing slash `Url::join` needs.
fn base_of(url: &str) -> Result<Url, Error> {
    let mut base = Url::parse(url)
        .map_err(|err| Error::Crawl(format!("the CKAN URL `{url}` is not a URL: {err}")))?;
    if !base.path().ends_with('/') {
        let path = format!("{}/", base.path());
        base.set_path(&path);
    }
    base.set_query(None);
    base.set_fragment(None);
    Ok(base)
}

async fn search_page(
    crawler: &Crawler,
    base: &Url,
    organizations: &[String],
    start: u64,
) -> Result<SearchPage, Error> {
    let mut url = base
        .join("api/3/action/package_search")
        .map_err(|err| Error::Crawl(format!("the CKAN search URL: {err}")))?;
    url.query_pairs_mut()
        .append_pair("q", "*:*")
        .append_pair("rows", &ROWS.to_string())
        .append_pair("start", &start.to_string())
        // A stable order, so a dataset added while the listing pages is not read twice or
        // skipped by the others moving.
        .append_pair("sort", "name asc")
        .append_pair("include_private", "false");
    if !organizations.is_empty() {
        // The names are CKAN names (jc-core checks them), so they need no Solr escaping.
        url.query_pairs_mut().append_pair(
            "fq",
            &format!("organization:({})", organizations.join(" OR ")),
        );
    }
    let host = base.host_str().unwrap_or_default().to_owned();
    let body = match fetch_url(
        &crawler.client,
        &crawler.policy,
        &host,
        &url,
        MAX_PAGE_BYTES,
        None,
        None,
    )
    .await
    {
        FetchOutcome::Success { status, body, .. } if status.is_success() => body,
        FetchOutcome::Success { status, .. } | FetchOutcome::Status(status) => {
            return Err(Error::Crawl(format!(
                "the catalogue answered {status} to {url}"
            )))
        }
        FetchOutcome::Refused => {
            return Err(Error::Crawl(format!(
                "{url} is not a public https address, so it is not read"
            )))
        }
        FetchOutcome::TooLarge => {
            return Err(Error::Crawl(format!("{url} answered more than 10 MiB")))
        }
        FetchOutcome::RedirectExternal { target_url } => {
            return Err(Error::Crawl(format!(
                "{url} redirected to another host, {target_url}"
            )))
        }
        FetchOutcome::NotModified { .. } => {
            return Err(Error::Crawl(format!("{url} answered 304 to a plain GET")))
        }
        FetchOutcome::Failed(why) => return Err(Error::Crawl(format!("{url}: {why}"))),
    };
    let envelope: Envelope<SearchPage> = serde_json::from_slice(&body)
        .map_err(|err| Error::Crawl(format!("{url} is not a CKAN answer: {err}")))?;
    match envelope {
        Envelope {
            success: true,
            result: Some(page),
        } => Ok(page),
        _ => Err(Error::Crawl(format!("the catalogue refused {url}"))),
    }
}

/// One catalogue to read, and where its datasets go.
#[derive(Debug, Clone, Copy)]
pub struct Catalogue<'a> {
    /// The project the source belongs to.
    pub project: &'a str,
    /// The site row of the source ([`crate::crawl::upsert_site`]).
    pub site_id: i64,
    /// The `CkanInstance`'s URL.
    pub url: &'a str,
    /// The source's `maxPages`: the most datasets it holds.
    pub max_datasets: u32,
    /// The source's language when it declares exactly one; detected otherwise.
    pub language: Option<&'a str>,
    /// The source's `organizations`: only their datasets are read; empty reads every one.
    pub organizations: &'a [String],
}

/// A page of the catalogue as it is written to the index.
struct Written<'a> {
    url: &'a str,
    hash: &'a str,
    modified: Option<&'a str>,
}

/// Writes one page of the catalogue's site and returns its id, for the sink.
async fn store(pool: &PgPool, catalogue: Catalogue<'_>, page: Written<'_>) -> Result<i64, Error> {
    let mut tx = crate::project_scope(pool, catalogue.project).await?;
    let page_id: i64 = sqlx::query_scalar(
        "INSERT INTO pages (site_id, project, url, depth, status, content_hash, language, included, last_modified, fetched_at) \
         VALUES ($1, $2, $3, 0, 'fetched', $4, $5, true, $6, now()) \
         ON CONFLICT (site_id, url) DO UPDATE SET status = 'fetched', content_hash = EXCLUDED.content_hash, \
             language = EXCLUDED.language, last_modified = EXCLUDED.last_modified, fetched_at = now() \
         RETURNING id",
    )
    .bind(catalogue.site_id)
    .bind(catalogue.project)
    .bind(page.url)
    .bind(page.hash)
    .bind(catalogue.language)
    .bind(page.modified)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(page_id)
}

/// What each indexed page was written from: its `last_modified` and content hash, by URL.
type Known = HashMap<String, (Option<String>, Option<String>)>;

/// Whether the page at `url` was written from this `modified` and this content (T-4366).
fn unchanged(known: &Known, url: &str, modified: &Option<String>, hash: &str) -> bool {
    known.get(url).is_some_and(|(held_modified, held)| {
        held_modified.is_some() && held_modified == modified && held.as_deref() == Some(hash)
    })
}

/// Writes the how-to the interfaces share, once, at the catalogue's own address (T-4747), when
/// it changed; returns that address, which the sweep keeps.
async fn store_connect(
    pool: &PgPool,
    catalogue: Catalogue<'_>,
    base: &Url,
    known: &Known,
    html: &str,
    sink: &mut impl Sink,
) -> Result<String, Error> {
    let url = base.to_string();
    let hash = hex::encode(Sha256::digest(html.as_bytes()));
    if known
        .get(&url)
        .is_none_or(|(_, held)| held.as_deref() != Some(hash.as_str()))
    {
        let written = Written {
            url: &url,
            hash: &hash,
            modified: None,
        };
        let page_id = store(pool, catalogue, written).await?;
        sink.page(page_id, &url, catalogue.language, html).await;
    }
    Ok(url)
}

/// Reads the public datasets of `catalogue` into its site, handing each new or changed dataset
/// to `sink` as a page.
pub async fn sync_ckan(
    pool: &PgPool,
    crawler: &Crawler,
    catalogue: Catalogue<'_>,
    sink: &mut impl Sink,
) -> Result<CkanReport, Error> {
    let Catalogue {
        project,
        site_id,
        url: ckan_url,
        max_datasets,
        language,
        organizations,
    } = catalogue;
    let base = base_of(ckan_url)?;
    // What each indexed dataset was written from, and the page it became: a dataset is written
    // again when either changed, so a new page format reaches the unchanged ones too (T-4366).
    let known: Known = {
        let mut tx = crate::project_scope(pool, project).await?;
        let rows: Vec<(String, Option<String>, Option<String>)> =
            sqlx::query_as("SELECT url, last_modified, content_hash FROM pages WHERE site_id = $1")
                .bind(site_id)
                .fetch_all(&mut *tx)
                .await?;
        tx.rollback().await?;
        rows.into_iter()
            .map(|(url, modified, hash)| (url, (modified, hash)))
            .collect()
    };

    let mut report = CkanReport::default();
    let mut listed: HashSet<String> = HashSet::new();
    let mut connect: Option<String> = None;
    let mut start = 0;
    'listing: loop {
        let page = search_page(crawler, &base, organizations, start).await?;
        let returned = page.results.len() as u64;
        for dataset in page.results {
            if listed.len() >= max_datasets as usize {
                break 'listing;
            }
            if dataset.private || !is_dataset_name(&dataset.name) {
                continue;
            }
            let Ok(url) = base.join(&format!("dataset/{}", dataset.name)) else {
                continue;
            };
            let url = url.to_string();
            if !listed.insert(url.clone()) {
                continue;
            }
            if connect.is_none() {
                connect = connect_html(&dataset);
            }
            let html = dataset_html(&dataset);
            let hash = hex::encode(Sha256::digest(html.as_bytes()));
            if unchanged(&known, &url, &dataset.metadata_modified, &hash) {
                continue;
            }
            let written = Written {
                url: &url,
                hash: &hash,
                modified: dataset.metadata_modified.as_deref(),
            };
            let page_id = store(pool, catalogue, written).await?;
            sink.page(page_id, &url, language, &html).await;
            report.changed += 1;
        }
        start += ROWS;
        if returned == 0 || start >= page.count {
            break;
        }
    }
    report.datasets = listed.len();

    if let Some(html) = connect {
        listed.insert(store_connect(pool, catalogue, &base, &known, &html, sink).await?);
    }

    // The whole listing was read: what it no longer names goes, with its passages.
    let listed: Vec<String> = listed.into_iter().collect();
    let mut tx = crate::project_scope(pool, project).await?;
    report.removed = sqlx::query("DELETE FROM pages WHERE site_id = $1 AND NOT (url = ANY($2))")
        .bind(site_id)
        .bind(&listed)
        .execute(&mut *tx)
        .await?
        .rows_affected() as usize;
    sqlx::query("UPDATE sites SET last_crawl = now() WHERE id = $1")
        .bind(site_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dataset_reads_as_one_escaped_page() {
        let dataset: Dataset = serde_json::from_value(serde_json::json!({
            "name": "air-quality",
            "title": "Air quality <live>",
            "notes": "Hourly readings.\n\nFrom five stations & a van.",
            "organization": {"title": "Environment"},
            "tags": [{"display_name": "air"}, {"name": "no2"}, {}],
            "resources": [
                {"name": "Readings", "format": "CSV", "description": "One row per hour",
                 "url": "https://files.example.org/readings.csv"},
                {"format": "API", "url": "https://dev.example.org/api/endpoint/od-air/ngsi-ld/v1/entities?type=A&x=<y>"}
            ],
            "extra_field_ckan_adds": 1
        }))
        .expect("a dataset");
        let html = dataset_html(&dataset);
        assert!(html.contains("<h1>Air quality &lt;live&gt;</h1>"));
        assert!(html.contains("<p>Hourly readings.</p><p>From five stations &amp; a van.</p>"));
        assert!(html.contains("<p>Publisher: Environment</p>"));
        assert!(html.contains("<p>Keywords: air, no2</p>"));
        // A plain file's address is noise to a reader; an Endpoint's lets its citation find this
        // page (T-4366), escaped like every other value.
        assert!(html.contains("<li>Readings (CSV): One row per hour</li>"));
        assert!(html.contains(
            "<li>Unnamed resource (API) (https://dev.example.org/api/endpoint/od-air/ngsi-ld/v1/entities?type=A&amp;x=&lt;y&gt;)</li>"
        ));
    }

    /// T-4747: the how-to every interface resource carries is the same on every dataset page, so
    /// a question about it cited whichever dataset ranked first. A dataset page keeps each
    /// interface's name, format and addresses (the typed example finds the page by its type,
    /// T-4625); the how-to is one page of the catalogue's own.
    #[test]
    fn the_interface_how_to_is_one_catalogue_page_not_every_datasets() {
        let dataset = |name: &str, slug: &str, kind: &str| -> Dataset {
            serde_json::from_value(serde_json::json!({
                "name": name,
                "title": name,
                "resources": [
                    {"name": "NGSI-LD API", "format": "NGSI-LD",
                     "url": format!("https://gw.example/api/endpoint/{slug}/ngsi-ld/v1/"),
                     "description": format!("The NGSI-LD API of this dataset.\n\n    curl 'https://gw.example/api/endpoint/{slug}/ngsi-ld/v1/entities?type={kind}'")},
                    {"name": "Model Context Protocol", "format": "MCP",
                     "url": format!("https://gw.example/api/endpoint/{slug}/mcp"),
                     "description": format!("For AI assistants: a Model Context Protocol server over https://gw.example/api/endpoint/{slug}. Add it to Claude as a connector.")},
                    {"name": "Readings", "format": "CSV", "description": "One row per hour",
                     "url": "https://files.example.org/readings.csv"}
                ]
            }))
            .expect("a dataset")
        };
        let html = dataset_html(&dataset("skoly", "bbsk-all", "School"));
        assert!(
            !html.contains("AI assistants") && !html.contains("Add it to Claude"),
            "{html}"
        );
        assert!(html.contains(
            "<li>NGSI-LD API (NGSI-LD) (https://gw.example/api/endpoint/bbsk-all/ngsi-ld/v1/) (https://gw.example/api/endpoint/bbsk-all/ngsi-ld/v1/entities?type=School)</li>"
        ), "{html}");
        assert!(html.contains("<li>Model Context Protocol (MCP) (https://gw.example/api/endpoint/bbsk-all/mcp) (https://gw.example/api/endpoint/bbsk-all)</li>"), "{html}");
        assert!(
            html.contains("<li>Readings (CSV): One row per hour</li>"),
            "a plain file keeps its description"
        );

        let page = connect_html(&dataset("skoly", "bbsk-all", "School")).expect("a how-to page");
        assert!(page.contains("Add it to Claude as a connector."), "{page}");
        assert!(
            !page.contains("bbsk-all") && !page.contains("type=School"),
            "the catalogue's page names no dataset's Endpoint or type: {page}"
        );
        assert!(
            page.contains("/api/endpoint/{endpoint}/ngsi-ld/v1/entities?type={type}"),
            "{page}"
        );
        let plain: Dataset = serde_json::from_value(serde_json::json!({
            "name": "x", "resources": [{"name": "Data", "format": "CSV"}]
        }))
        .expect("a dataset");
        assert_eq!(connect_html(&plain), None, "no interface, no how-to page");
    }

    #[test]
    fn only_a_ckan_name_becomes_a_citation_path() {
        for name in ["ab", "air-quality", "bus_stops_2026"] {
            assert!(is_dataset_name(name), "{name}");
        }
        for name in ["a", "Air", "../admin", "a b", "x?y", &"a".repeat(101)] {
            assert!(!is_dataset_name(name), "{name}");
        }
    }

    #[test]
    fn the_base_url_keeps_its_path_and_gains_a_slash() {
        assert_eq!(
            base_of("https://data.example/catalogue?x=1")
                .expect("url")
                .as_str(),
            "https://data.example/catalogue/"
        );
        assert_eq!(
            base_of("https://data.example").expect("url").as_str(),
            "https://data.example/"
        );
        assert!(base_of("not a url").is_err());
    }
}
