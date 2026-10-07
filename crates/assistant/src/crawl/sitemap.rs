//! Sitemap discovery and XML parsing using `quick-xml` (T-3052).

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
}

/// Parses `<loc>` elements from `<urlset>` and `<sitemapindex>` XML documents.
pub fn parse_sitemap_xml(xml: &str) -> ParsedSitemap {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut result = ParsedSitemap::default();
    let mut inside_url = false;
    let mut inside_sitemap = false;
    let mut inside_loc = false;
    let mut current_text = String::new();

    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => match e.local_name().as_ref() {
                "url" => inside_url = true,
                "sitemap" => inside_sitemap = true,
                "loc" => {
                    inside_loc = true;
                    current_text.clear();
                }
                _ => {}
            },
            Ok(Event::Text(ref e)) if inside_loc => current_text.push_str(&e.xml10_content()),
            // quick-xml 0.42 reports `&amp;` and the other references as events of their own;
            // a sitemap URL with a query string is full of them.
            Ok(Event::GeneralRef(ref e)) if inside_loc => {
                if let Ok(text) = quick_xml::escape::unescape(&format!("&{};", e.as_ref() as &str))
                {
                    current_text.push_str(&text);
                }
            }
            Ok(Event::End(ref e)) => match e.local_name().as_ref() {
                "url" => inside_url = false,
                "sitemap" => inside_sitemap = false,
                "loc" => {
                    inside_loc = false;
                    let trimmed = current_text.trim();
                    if !trimmed.is_empty() {
                        if inside_url {
                            result.urls.push(trimmed.to_string());
                        } else if inside_sitemap {
                            result.sub_sitemaps.push(trimmed.to_string());
                        }
                    }
                    current_text.clear();
                }
                _ => {}
            },
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
) -> Vec<Url> {
    let mut collected = Vec::new();
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
                }
            }
        }
    }

    collected
}

fn add_filtered_urls(
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
        let mut u = match Url::parse(u_str) {
            Ok(u) => u,
            Err(_) => continue,
        };
        if u.host_str() != Some(site_host) {
            continue;
        }
        if !is_url_allowed(&u, policy.allow_plain_http) {
            continue;
        }
        u.set_fragment(None);
        if u.scheme() == "http" && u.port() == Some(80) {
            let _ = u.set_port(None);
        }
        if u.scheme() == "https" && u.port() == Some(443) {
            let _ = u.set_port(None);
        }
        if !target.contains(&u) {
            target.push(u);
        }
    }
}
