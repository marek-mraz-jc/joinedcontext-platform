//! Sitemap discovery and XML parsing using `quick-xml` (T-3052).

use std::collections::HashMap;

use quick_xml::events::Event;
use quick_xml::reader::Reader;
use reqwest::Client;
use url::Url;

use super::fetch::{fetch_url, is_url_allowed, FetchOutcome};
use super::CrawlPolicy;

/// Parsed URLs from a sitemap or sitemap index.
#[derive(Debug, Default)]
pub struct ParsedSitemap {
    /// URLs found in `<urlset><url><loc>`.
    pub urls: Vec<String>,
    /// Sub-sitemaps found in `<sitemapindex><sitemap><loc>`.
    pub sub_sitemaps: Vec<String>,
    /// The `<lastmod>` of each `<url>` that states one, by its `<loc>` (T-4749).
    pub lastmod: HashMap<String, String>,
}

/// The pages a site's sitemaps list, and when each last changed where the sitemap says so.
#[derive(Debug, Default)]
pub struct Listed {
    /// Same-host page URLs, canonical, in sitemap order.
    pub urls: Vec<Url>,
    /// Each listed URL's `<lastmod>` as an RFC 3339 instant in UTC, by its canonical URL.
    pub lastmod: HashMap<String, String>,
}

/// Where the parser stands in a sitemap: the element it is inside and the text read so far.
#[derive(Default)]
struct Cursor {
    inside_url: bool,
    inside_sitemap: bool,
    /// Inside `<loc>` or `<lastmod>`, whose text is collected.
    reading: bool,
    text: String,
    loc: String,
    lastmod: String,
}

impl Cursor {
    fn open(&mut self, name: &[u8]) {
        match name {
            b"url" => {
                self.inside_url = true;
                self.loc.clear();
                self.lastmod.clear();
            }
            b"sitemap" => self.inside_sitemap = true,
            b"loc" | b"lastmod" => {
                self.reading = true;
                self.text.clear();
            }
            _ => {}
        }
    }

    fn close(&mut self, name: &[u8], into: &mut ParsedSitemap) {
        let text = std::mem::take(&mut self.text).trim().to_string();
        match name {
            b"url" => {
                self.inside_url = false;
                if !self.loc.is_empty() && !self.lastmod.is_empty() {
                    into.lastmod.insert(
                        std::mem::take(&mut self.loc),
                        std::mem::take(&mut self.lastmod),
                    );
                }
            }
            b"sitemap" => self.inside_sitemap = false,
            b"lastmod" => {
                self.reading = false;
                self.lastmod = text;
            }
            b"loc" => {
                self.reading = false;
                if text.is_empty() {
                    return;
                }
                if self.inside_url {
                    self.loc.clone_from(&text);
                    into.urls.push(text);
                } else if self.inside_sitemap {
                    into.sub_sitemaps.push(text);
                }
            }
            _ => {}
        }
    }
}

/// Parses `<loc>` and `<lastmod>` elements from `<urlset>` and `<sitemapindex>` XML documents.
pub fn parse_sitemap_xml(xml: &str) -> ParsedSitemap {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut result = ParsedSitemap::default();
    let mut at = Cursor::default();
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => at.open(e.local_name().as_ref()),
            Ok(Event::Text(ref e)) if at.reading => {
                if let Ok(text) = e.xml10_content() {
                    at.text.push_str(&text);
                }
            }
            // quick-xml reports `&amp;` and the other references as events of their own;
            // a sitemap URL with a query string is full of them.
            Ok(Event::GeneralRef(ref e)) if at.reading => {
                if let Some(text) = e.decode().ok().and_then(|name| {
                    quick_xml::escape::unescape(&format!("&{name};"))
                        .ok()
                        .map(|text| text.into_owned())
                }) {
                    at.text.push_str(&text);
                }
            }
            Ok(Event::End(ref e)) => at.close(e.local_name().as_ref(), &mut result),
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    result
}

/// Fetches sitemap URLs, follows sub-sitemaps one level deep, and filters to same-host URLs.
pub async fn fetch_sitemap_urls(
    client: &Client,
    policy: &CrawlPolicy,
    site_host: &str,
    initial_sitemaps: &[String],
    max_pages: u32,
) -> Listed {
    let mut collected = Vec::new();
    let mut lastmod = HashMap::new();
    let max = max_pages as usize;

    for sitemap_str in initial_sitemaps {
        if collected.len() >= max {
            break;
        }
        let sitemap_url = match Url::parse(sitemap_str) {
            Ok(u) => u,
            Err(_) => continue,
        };
        if sitemap_url.host_str() != Some(site_host) {
            continue;
        }
        if !is_url_allowed(&sitemap_url, policy.allow_plain_http) {
            continue;
        }

        let outcome = fetch_url(
            client,
            policy,
            site_host,
            &sitemap_url,
            policy.max_page_bytes,
            None,
            None,
        )
        .await;

        if let FetchOutcome::Success { body, .. } = outcome {
            let text = String::from_utf8_lossy(&body);
            let parsed = parse_sitemap_xml(&text);
            add_filtered_urls(&mut collected, &parsed.urls, site_host, policy, max);
            dated(&mut lastmod, parsed.lastmod);

            for sub_str in &parsed.sub_sitemaps {
                if collected.len() >= max {
                    break;
                }
                let sub_url = match Url::parse(sub_str) {
                    Ok(u) => u,
                    Err(_) => continue,
                };
                if sub_url.host_str() != Some(site_host) {
                    continue;
                }
                if !is_url_allowed(&sub_url, policy.allow_plain_http) {
                    continue;
                }
                let sub_outcome = fetch_url(
                    client,
                    policy,
                    site_host,
                    &sub_url,
                    policy.max_page_bytes,
                    None,
                    None,
                )
                .await;
                if let FetchOutcome::Success { body: sub_body, .. } = sub_outcome {
                    let sub_text = String::from_utf8_lossy(&sub_body);
                    let sub_parsed = parse_sitemap_xml(&sub_text);
                    add_filtered_urls(&mut collected, &sub_parsed.urls, site_host, policy, max);
                    dated(&mut lastmod, sub_parsed.lastmod);
                }
            }
        }
    }

    // Only what was kept: a date of a URL left out is no one's to read.
    let kept: std::collections::HashSet<&str> = collected.iter().map(Url::as_str).collect();
    lastmod.retain(|url, _| kept.contains(url.as_str()));
    Listed {
        urls: collected,
        lastmod,
    }
}

/// Adds each `<lastmod>` that reads as a date, keyed by its URL in canonical form.
fn dated(into: &mut HashMap<String, String>, lastmod: HashMap<String, String>) {
    for (loc, date) in lastmod {
        let (Ok(url), Some(at)) = (Url::parse(&loc), super::meta::instant(&date)) else {
            continue;
        };
        into.insert(canonical(url).to_string(), at);
    }
}

/// The URL without its fragment and its scheme's default port, as the crawl's frontier keys it.
fn canonical(mut u: Url) -> Url {
    u.set_fragment(None);
    if u.scheme() == "http" && u.port() == Some(80) {
        let _ = u.set_port(None);
    }
    if u.scheme() == "https" && u.port() == Some(443) {
        let _ = u.set_port(None);
    }
    u
}

pub(super) fn add_filtered_urls(
    target: &mut Vec<Url>,
    raw_urls: &[String],
    site_host: &str,
    policy: &CrawlPolicy,
    max: usize,
) {
    for u_str in raw_urls {
        if target.len() >= max {
            break;
        }
        let u = match Url::parse(u_str) {
            Ok(u) => u,
            Err(_) => continue,
        };
        if u.host_str() != Some(site_host) {
            continue;
        }
        if !is_url_allowed(&u, policy.allow_plain_http) {
            continue;
        }
        let u = canonical(u);
        if !target.contains(&u) {
            target.push(u);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// T-4749: each `<url>`'s `<lastmod>` is kept by its `<loc>`; a URL without one has none.
    #[test]
    fn each_url_keeps_its_own_lastmod() {
        let parsed = parse_sitemap_xml(
            r#"<?xml version="1.0"?><urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">
            <url><lastmod>2026-03-12</lastmod><loc>https://www.city.test/a?x=1&amp;y=2</loc></url>
            <url><loc>https://www.city.test/b</loc></url>
            <url><loc>https://www.city.test/c</loc><lastmod>2026-10-01T08:00:00+02:00</lastmod></url>
            </urlset>"#,
        );
        assert_eq!(parsed.urls.len(), 3);
        assert_eq!(
            parsed.lastmod,
            HashMap::from([
                (
                    "https://www.city.test/a?x=1&y=2".to_owned(),
                    "2026-03-12".to_owned()
                ),
                (
                    "https://www.city.test/c".to_owned(),
                    "2026-10-01T08:00:00+02:00".to_owned()
                ),
            ])
        );
        let mut into = HashMap::new();
        dated(&mut into, parsed.lastmod);
        assert_eq!(
            into.get("https://www.city.test/c").map(String::as_str),
            Some("2026-10-01T06:00:00Z")
        );
    }
}
