//! What a page states of itself (AG-120, T-4659): its title, description and the dates it was
//! published and last changed, read from its own markup at crawl time. A value the page does not
//! state stays `None`, never guessed; a search hit for an MCP server carries them.

use scraper::{Html, Selector};
use serde_json::Value;
use time::format_description::well_known::{Rfc2822, Rfc3339};
use time::macros::format_description;
use time::{Date, OffsetDateTime, PrimitiveDateTime, UtcOffset};

/// The longest title kept, in characters.
const MAX_TITLE: usize = 300;
/// The longest description kept, in characters.
const MAX_DESCRIPTION: usize = 1_000;

/// A page's own metadata; each date an RFC 3339 instant in UTC.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Meta {
    /// The two-letter code of `<html lang>`.
    pub language: Option<String>,
    pub title: Option<String>,
    pub description: Option<String>,
    pub published: Option<String>,
    pub modified: Option<String>,
}

/// The first non-empty `content` of the `<meta>` elements `selectors` find, in that order.
fn meta(document: &Html, selectors: &[&str]) -> Option<String> {
    selectors.iter().find_map(|s| {
        let selector = Selector::parse(s).ok()?;
        document
            .select(&selector)
            .filter_map(|e| e.value().attr("content"))
            .map(squeeze)
            .find(|text| !text.is_empty())
    })
}

/// Whitespace runs as one space, the ends trimmed.
fn squeeze(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn capped(text: String, most: usize) -> String {
    text.chars().take(most).collect()
}

/// The first string member `key` anywhere in a JSON-LD document: its objects, its arrays and
/// its `@graph`, depth first.
fn json_ld(value: &Value, key: &str) -> Option<String> {
    match value {
        Value::Object(map) => map
            .get(key)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| map.values().find_map(|v| json_ld(v, key))),
        Value::Array(items) => items.iter().find_map(|v| json_ld(v, key)),
        _ => None,
    }
}

/// `key` of the page's JSON-LD blocks, the first block that names it.
fn linked_data(document: &Html, key: &str) -> Option<String> {
    let selector = Selector::parse(r#"script[type="application/ld+json"]"#).ok()?;
    document.select(&selector).find_map(|script| {
        let text: String = script.text().collect();
        let value: Value = serde_json::from_str(text.trim()).ok()?;
        json_ld(&value, key)
    })
}

/// The first date the `<meta>` elements `selectors` find, then the JSON-LD `key`, that reads as
/// an instant: a tag in a form [`instant`] does not read leaves the next one its turn (T-4749).
fn dated(document: &Html, selectors: &[&str], key: &str) -> Option<String> {
    selectors
        .iter()
        .find_map(|s| meta(document, &[s]).as_deref().and_then(instant))
        .or_else(|| linked_data(document, key).as_deref().and_then(instant))
}

/// A date as an RFC 3339 instant in UTC: RFC 3339 itself, a date and time without an offset
/// (read as UTC), a date alone (its midnight in UTC), or an HTTP date (`Last-Modified`). `None`
/// for anything else.
pub fn instant(text: &str) -> Option<String> {
    let text = text.trim();
    let at = OffsetDateTime::parse(text, &Rfc3339)
        .or_else(|_| OffsetDateTime::parse(text, &Rfc2822))
        // ISO 8601 with its offset written without a colon, as hel.fi's article tags have it.
        .or_else(|_| {
            OffsetDateTime::parse(
                text,
                format_description!(
                    "[year]-[month]-[day]T[hour]:[minute]:[second][offset_hour sign:mandatory][offset_minute]"
                ),
            )
        })
        .or_else(|_| {
            PrimitiveDateTime::parse(
                text,
                format_description!("[year]-[month]-[day]T[hour]:[minute]:[second]"),
            )
            .or_else(|_| {
                PrimitiveDateTime::parse(
                    text,
                    format_description!("[year]-[month]-[day] [hour]:[minute]:[second]"),
                )
            })
            .or_else(|_| {
                PrimitiveDateTime::parse(
                    text,
                    format_description!("[year]-[month]-[day]T[hour]:[minute]"),
                )
            })
            .map(PrimitiveDateTime::assume_utc)
        })
        .or_else(|_| {
            Date::parse(text, format_description!("[year]-[month]-[day]"))
                .map(|date| date.midnight().assume_utc())
        })
        .ok()?;
    at.to_offset(UtcOffset::UTC)
        .format(format_description!(
            "[year]-[month]-[day]T[hour]:[minute]:[second]Z"
        ))
        .ok()
}

/// A PDF date, `D:YYYYMMDDHHmmSSOHH'mm'` with everything after the day optional, as
/// [`instant`] reads it; a date without an offset is UTC.
fn pdf_date(text: &str) -> Option<String> {
    let text = text.trim();
    let text = text.strip_prefix("D:").unwrap_or(text);
    let digits = text.bytes().take_while(u8::is_ascii_digit).count();
    if digits < 8 {
        return None;
    }
    let (date, rest) = text.split_at(digits);
    let part = |at: usize| date.get(at..at + 2).unwrap_or("00");
    let offset = match rest.as_bytes().first() {
        Some(sign @ (b'+' | b'-')) => {
            let hm: String = rest[1..].chars().filter(char::is_ascii_digit).collect();
            format!(
                "{}{}:{}",
                *sign as char,
                hm.get(..2)?,
                hm.get(2..4).unwrap_or("00")
            )
        }
        _ => "Z".to_owned(),
    };
    instant(&format!(
        "{}-{}-{}T{}:{}:{}{offset}",
        &date[..4],
        part(4),
        part(6),
        part(8),
        part(10),
        part(12)
    ))
}

/// A PDF's own title, subject and dates from its document information dictionary (AG-120);
/// nothing for a file that has none or is no PDF.
pub fn pdf(bytes: &[u8]) -> Meta {
    let Ok(info) = lopdf::Document::load_metadata_mem(bytes) else {
        return Meta::default();
    };
    let text = |value: Option<String>, most: usize| {
        value
            .map(|v| squeeze(&v))
            .filter(|v| !v.is_empty())
            .map(|v| capped(v, most))
    };
    Meta {
        language: None,
        title: text(info.title, MAX_TITLE),
        description: text(info.subject, MAX_DESCRIPTION),
        published: info.creation_date.as_deref().and_then(pdf_date),
        modified: info.modification_date.as_deref().and_then(pdf_date),
    }
}

/// The page's own title, description and dates (AG-120).
pub fn page(html: &str) -> Meta {
    let document = Html::parse_document(html);
    let title = meta(&document, &[r#"meta[property="og:title"]"#]).or_else(|| {
        let selector = Selector::parse("title").ok()?;
        let text = squeeze(
            &document
                .select(&selector)
                .next()?
                .text()
                .collect::<String>(),
        );
        (!text.is_empty()).then_some(text)
    });
    let description = meta(
        &document,
        &[
            r#"meta[name="description" i]"#,
            r#"meta[property="og:description"]"#,
        ],
    );
    let published = dated(
        &document,
        &[
            r#"meta[property="article:published_time"]"#,
            r#"meta[name="dcterms.created" i]"#,
            r#"meta[name="DC.date.created" i]"#,
        ],
        "datePublished",
    );
    let modified = dated(
        &document,
        &[
            r#"meta[property="article:modified_time"]"#,
            r#"meta[name="dcterms.modified" i]"#,
            r#"meta[name="DC.date.modified" i]"#,
        ],
        "dateModified",
    );
    Meta {
        language: super::links::extract_language(html),
        title: title.map(|t| capped(t, MAX_TITLE)),
        description: description.map(|d| capped(d, MAX_DESCRIPTION)),
        published,
        modified,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_open_graph_and_article_tags_come_first() {
        let html = r#"<html lang="sk"><head><title>Mesto | Klientske centrum</title>
            <meta property="og:title" content=" Klientske   centrum ">
            <meta name="Description" content="Úradné hodiny a kontakty">
            <meta property="og:description" content="iné">
            <meta property="article:published_time" content="2024-03-01T10:00:00+01:00">
            <meta property="article:modified_time" content="2026-09-30">
            </head><body></body></html>"#;
        assert_eq!(
            page(html),
            Meta {
                language: Some("sk".into()),
                title: Some("Klientske centrum".into()),
                description: Some("Úradné hodiny a kontakty".into()),
                published: Some("2024-03-01T09:00:00Z".into()),
                modified: Some("2026-09-30T00:00:00Z".into()),
            }
        );
    }

    #[test]
    fn the_title_element_json_ld_and_dublin_core_stand_in() {
        let html = r#"<html><head><title>
              Kúpalisko   Štiavničky </title>
            <meta name="dcterms.created" content="2023-05-02">
            <script type="application/ld+json">{"@context":"https://schema.org","@graph":[{"@type":"WebSite"},{"@type":"WebPage","dateModified":"2026-08-01T07:30:00Z"}]}</script>
            <script type="application/ld+json">not json</script>
            </head></html>"#;
        let meta = page(html);
        assert_eq!(meta.title.as_deref(), Some("Kúpalisko Štiavničky"));
        assert_eq!(meta.description, None, "never guessed");
        assert_eq!(meta.published.as_deref(), Some("2023-05-02T00:00:00Z"));
        assert_eq!(meta.modified.as_deref(), Some("2026-08-01T07:30:00Z"));
    }

    #[test]
    fn a_page_that_states_nothing_has_nothing() {
        assert_eq!(page("<p>Ahoj</p>"), Meta::default());
        assert_eq!(
            page(r#"<title>  </title><meta name="description" content="">"#),
            Meta::default()
        );
        let long = format!("<title>{}</title>", "á".repeat(400));
        assert_eq!(
            page(&long).title.map(|t| t.chars().count()),
            Some(MAX_TITLE)
        );
    }

    #[test]
    fn a_pdf_states_its_document_information() {
        use lopdf::{dictionary, Document, Object, StringFormat};
        // Text outside PDFDocEncoding is UTF-16BE behind its byte order mark.
        let utf16 = |text: &str| {
            let mut bytes = vec![0xFE, 0xFF];
            bytes.extend(text.encode_utf16().flat_map(u16::to_be_bytes));
            Object::String(bytes, StringFormat::Hexadecimal)
        };
        let mut document = Document::with_version("1.5");
        let pages =
            document.add_object(dictionary! {"Type" => "Pages", "Kids" => vec![], "Count" => 0});
        let catalog = document.add_object(dictionary! {"Type" => "Catalog", "Pages" => pages});
        let info = document.add_object(dictionary! {
            "Title" => utf16("Rozpočet  mesta 2026"),
            "Subject" => utf16("Návrh"),
            "CreationDate" => Object::string_literal("D:20251201093000+01'00'"),
            "ModDate" => Object::string_literal("D:20260115"),
        });
        document.trailer.set("Root", catalog);
        document.trailer.set("Info", info);
        let mut bytes = Vec::new();
        document.save_to(&mut bytes).expect("a PDF");
        let meta = pdf(&bytes);
        assert_eq!(meta.description.as_deref(), Some("Návrh"));
        assert_eq!(meta.published.as_deref(), Some("2025-12-01T08:30:00Z"));
        assert_eq!(meta.modified.as_deref(), Some("2026-01-15T00:00:00Z"));
        assert_eq!(meta.title.as_deref(), Some("Rozpočet mesta 2026"));
        assert_eq!(pdf(b"%PDF-1.4 no trailer"), Meta::default());
        assert_eq!(pdf_date("D:2026"), None);
        assert_eq!(
            pdf_date("20260115103000Z").as_deref(),
            Some("2026-01-15T10:30:00Z")
        );
        assert_eq!(
            pdf_date("D:20260115103000-05'30").as_deref(),
            Some("2026-01-15T16:00:00Z")
        );
    }

    /// T-4749: a tag whose date does not read leaves the JSON-LD its turn.
    #[test]
    fn a_date_that_does_not_read_gives_way_to_the_next_that_does() {
        let stated = page(
            r#"<html><head>
            <meta property="article:modified_time" content="22.09.2026">
            <meta name="dcterms.modified" content="2026-09-21">
            <script type="application/ld+json">{"datePublished":"včera","dateModified":"2026-01-01"}</script>
            </head></html>"#,
        );
        assert_eq!(stated.modified.as_deref(), Some("2026-09-21T00:00:00Z"));
        assert_eq!(stated.published, None);
    }

    #[test]
    fn dates_read_in_every_usual_form_and_nothing_else() {
        assert_eq!(
            instant("Tue, 15 Nov 1994 08:12:31 GMT").as_deref(),
            Some("1994-11-15T08:12:31Z")
        );
        assert_eq!(
            instant("2026-09-30T08:12:00.123+02:00").as_deref(),
            Some("2026-09-30T06:12:00Z")
        );
        assert_eq!(
            instant("2026-09-30 08:12:00").as_deref(),
            Some("2026-09-30T08:12:00Z")
        );
        assert_eq!(
            instant("2026-09-30T08:12").as_deref(),
            Some("2026-09-30T08:12:00Z")
        );
        assert_eq!(
            instant("2026-09-22T13:58:36+0300").as_deref(),
            Some("2026-09-22T10:58:36Z")
        );
        for bad in ["", "včera", "2026-13-01", "30.09.2026", "2026"] {
            assert_eq!(instant(bad), None, "{bad}");
        }
    }
}
