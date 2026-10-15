//! A WordPress site's own content list as crawl addresses (T-4154, MF-51).
//!
//! Many WordPress themes keep a page's text in custom fields, so `content.rendered` of the REST
//! API is often empty: the API is read for each item's `link` only, and the page itself is
//! crawled like any other, under robots.txt, the patterns and `maxPages`.

use reqwest::Client;
use url::Url;

use super::fetch::{fetch_url, FetchOutcome};
use super::sitemap::add_filtered_urls;
use super::CrawlPolicy;

/// Items asked for per request: WordPress's own maximum.
pub const PER_PAGE: usize = 100;

/// The `link` of each item of a WordPress REST collection page; `None` when the body is not a
/// JSON array, which is how a site without the type (or without the API) answers.
pub fn parse_links(body: &[u8]) -> Option<Vec<String>> {
    let items: Vec<serde_json::Value> = serde_json::from_slice(body).ok()?;
    Some(
        items
            .iter()
            .filter_map(|item| item.get("link")?.as_str().map(str::to_owned))
            .collect(),
    )
}

/// The links of `types` from `start`'s host, each type newest change first, in the order the
/// types are listed, deduplicated, on the site's own host only, at most `max_pages` of them.
pub async fn fetch_wordpress_urls(
    client: &Client,
    policy: &CrawlPolicy,
    site_host: &str,
    start: &Url,
    types: &[String],
    max_pages: u32,
) -> Vec<Url> {
    let max = max_pages as usize;
    let mut collected = Vec::new();
    for base in types {
        let mut page = 1;
        while collected.len() < max {
            let Ok(url) = start.join(&format!(
                "/wp-json/wp/v2/{base}?per_page={PER_PAGE}&page={page}&orderby=modified&order=desc&_fields=link"
            )) else {
                break;
            };
            let outcome = fetch_url(
                client,
                policy,
                site_host,
                &url,
                policy.max_page_bytes,
                None,
                None,
            )
            .await;
            // Past the last page WordPress answers 400; any failure ends this type, never the crawl.
            let FetchOutcome::Success { body, .. } = outcome else {
                break;
            };
            let Some(links) = parse_links(&body) else {
                break;
            };
            add_filtered_urls(&mut collected, &links, site_host, policy, max);
            if links.len() < PER_PAGE {
                break;
            }
            page += 1;
        }
    }
    collected
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_are_read_from_an_array_and_anything_else_is_none() {
        let body = br#"[{"link":"https://www.city.test/a/"},{"id":2},{"link":7},{"link":"https://www.city.test/b/"}]"#;
        assert_eq!(
            parse_links(body),
            Some(vec![
                "https://www.city.test/a/".to_owned(),
                "https://www.city.test/b/".to_owned()
            ])
        );
        assert_eq!(parse_links(b"[]"), Some(vec![]));
        assert_eq!(
            parse_links(br#"{"code":"rest_no_route","data":{"status":404}}"#),
            None
        );
        assert_eq!(parse_links(b"<html>not json</html>"), None);
    }
}
