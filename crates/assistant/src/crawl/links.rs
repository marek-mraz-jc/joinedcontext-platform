//! HTML link and language extraction (T-3052).

use scraper::{Html, Selector};
use url::Url;

/// File extensions identified as documents.
pub const DOCUMENT_EXTENSIONS: &[&str] = &[
    ".pdf", ".doc", ".docx", ".odt", ".xls", ".xlsx", ".ods", ".ppt", ".pptx", ".odp", ".rtf",
];

/// The kind of extracted link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkKind {
    /// Same host, HTML page.
    Page,
    /// Document file (PDF, Office, etc.).
    Document,
    /// External host URL.
    External,
}

impl LinkKind {
    /// String representation stored in the database `links.kind` column.
    pub fn as_str(self) -> &'static str {
        match self {
            LinkKind::Page => "page",
            LinkKind::Document => "document",
            LinkKind::External => "external",
        }
    }
}

/// An extracted and classified link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractedLink {
    /// Canonical destination URL.
    pub url: Url,
    /// Classified link kind.
    pub kind: LinkKind,
}

/// Returns true if the URL path ends with a recognized document extension.
pub fn is_document_url(url: &Url) -> bool {
    let path = url.path().to_ascii_lowercase();
    DOCUMENT_EXTENSIONS.iter().any(|ext| path.ends_with(ext))
}

/// Returns true if a `Content-Type` (without its parameters) names a document rather than a page.
pub fn is_document_type(mime: &str) -> bool {
    let mime = mime.to_ascii_lowercase();
    matches!(
        mime.as_str(),
        "application/pdf"
            | "application/msword"
            | "application/rtf"
            | "text/rtf"
            | "application/vnd.ms-excel"
            | "application/vnd.ms-powerpoint"
    ) || mime.starts_with("application/vnd.openxmlformats-officedocument.")
        || mime.starts_with("application/vnd.oasis.opendocument.")
}

/// Extracts the two-letter ISO language code from `<html lang="...">`.
pub fn extract_language(html_text: &str) -> Option<String> {
    let document = Html::parse_document(html_text);
    let selector = Selector::parse("html[lang]").ok()?;
    let element = document.select(&selector).next()?;
    let lang = element.value().attr("lang")?.trim();
    // `get`, not indexing: the attribute is the site's text, and a multi-byte character at
    // byte 2 would make a slice panic.
    let code = lang.get(..2)?.to_ascii_lowercase();
    code.chars()
        .all(|c| c.is_ascii_alphabetic())
        .then_some(code)
}

/// Extracts all `<a href>` links from HTML, canonicalising and classifying each.
pub fn extract_links(base_url: &Url, html_text: &str, site_host: &str) -> Vec<ExtractedLink> {
    let document = Html::parse_document(html_text);
    let selector = match Selector::parse("a[href]") {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };

    let mut links = Vec::new();
    for element in document.select(&selector) {
        let href = match element.value().attr("href") {
            Some(h) => h.trim(),
            None => continue,
        };
        if href.starts_with("mailto:")
            || href.starts_with("tel:")
            || href.starts_with("javascript:")
            || href.to_ascii_lowercase().starts_with("mailto:")
            || href.to_ascii_lowercase().starts_with("tel:")
            || href.to_ascii_lowercase().starts_with("javascript:")
        {
            continue;
        }

        let mut resolved = match base_url.join(href) {
            Ok(u) => u,
            Err(_) => continue,
        };

        resolved.set_fragment(None);
        if resolved.scheme() == "http" && resolved.port() == Some(80) {
            let _ = resolved.set_port(None);
        }
        if resolved.scheme() == "https" && resolved.port() == Some(443) {
            let _ = resolved.set_port(None);
        }

        let kind = if is_document_url(&resolved) {
            LinkKind::Document
        } else if resolved.host_str() == Some(site_host) {
            LinkKind::Page
        } else {
            LinkKind::External
        };

        links.push(ExtractedLink {
            url: resolved,
            kind,
        });
    }
    links
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_are_resolved_against_the_page_and_classified() {
        let base = Url::parse("https://www.city.test/news/").expect("a test URL");
        let html = r##"<html><body>
            <a href="item#top">same site</a>
            <a href="https://www.city.test:443/plan.PDF">document</a>
            <a href="https://other.test/x">elsewhere</a>
            <a href="MAILTO:office@city.test">mail</a>
            <a href=" tel:+421 ">phone</a>
            <a href="JavaScript:void(0)">script</a>
            <a href="http://[bad">broken</a>
            <a>no href</a>
        </body></html>"##;
        let links: Vec<_> = extract_links(&base, html, "www.city.test")
            .into_iter()
            .map(|l| (l.url.to_string(), l.kind))
            .collect();
        assert_eq!(
            links,
            [
                ("https://www.city.test/news/item".to_owned(), LinkKind::Page),
                (
                    "https://www.city.test/plan.PDF".to_owned(),
                    LinkKind::Document
                ),
                ("https://other.test/x".to_owned(), LinkKind::External),
            ]
        );
    }

    #[test]
    fn a_page_without_links_has_none() {
        let base = Url::parse("https://www.city.test/").expect("a test URL");
        assert!(extract_links(&base, "", "www.city.test").is_empty());
    }
}
