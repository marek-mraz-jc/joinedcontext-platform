//! The canonical surface of one context space: its DCAT-AP record and the catalog of the
//! spaces a caller may discover (T-0167, SP-01, SP-03, SP-04, SP-10, SP-11).
//!
//! `GET /cs/{space}` is the one URL that hands a human, a program and an agent the same
//! entry point: the record's distributions are exactly the children SP-04 permits, so a
//! client learns the whole surface from one document instead of guessing paths.
//!
//! What the record never does is disclose existence. A space the caller holds no grant on
//! is absent from the catalog and answers 404 rather than 403, which is the same answer a
//! space that was never created gives (SP-06, SP-11, R20).

use crate::resolver::Space;
use serde_json::{json, Value};
use std::collections::BTreeMap;

/// The DCAT-AP media type the record is served as by default.
pub const JSON_LD: &str = "application/ld+json";
/// The RDF serialization a triple store or a partner platform asks for.
pub const TURTLE: &str = "text/turtle";
/// The representation a browser gets.
pub const HTML: &str = "text/html; charset=utf-8";

/// Which of the three representations of the same record the caller asked for (SP-10).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// DCAT-AP as JSON-LD, the default.
    JsonLd,
    /// The same graph as Turtle.
    Turtle,
    /// A page a person reads.
    Html,
}

impl Format {
    /// The media type this format is served as.
    pub fn media_type(self) -> &'static str {
        match self {
            Self::JsonLd => JSON_LD,
            Self::Turtle => TURTLE,
            Self::Html => HTML,
        }
    }
}

/// The format an `Accept` header asks for, JSON-LD when it asks for nothing recognisable.
///
/// Quality values are not weighed: the three types are alternatives rather than a
/// gradient, and the first one the header names is the one the client can read. A browser
/// sends `text/html` first and gets a page; `curl` sends `*/*` and gets the JSON-LD a
/// program can parse.
pub fn negotiate(accept: Option<&str>) -> Format {
    let Some(accept) = accept else {
        return Format::JsonLd;
    };
    for offer in accept.split(',') {
        match offer.split(';').next().unwrap_or_default().trim() {
            "text/turtle" => return Format::Turtle,
            "text/html" | "application/xhtml+xml" => return Format::Html,
            "application/ld+json" | "application/json" => return Format::JsonLd,
            _ => {}
        }
    }
    Format::JsonLd
}

/// The DCAT-AP dataset record of one space (SP-10).
///
/// `base` is the gateway's public URL when the deployment names one, so that the record
/// carries resolvable IRIs; without one the identifiers are the paths themselves, which
/// still name the same resources relative to the host that served the document.
pub fn dataset(space: &Space, base: &str) -> Value {
    let iri = format!("{base}/cs/{}", space.name());
    let mut record = json!({
        "@context": "https://www.w3.org/ns/dcat.jsonld",
        "@id": iri,
        "@type": "dcat:Dataset",
        "dct:identifier": space.name(),
        "dct:title": localized(&space.title, space.name()),
        "dcat:service": services(space, &iri),
    });
    if !space.description.is_empty() {
        record["dct:description"] = localized(&space.description, "");
    }
    if let Some(locale) = &space.default_locale {
        record["dct:language"] = json!(locale);
    }
    if space.is_sandbox {
        // A sandbox is thrown away on its TTL, so a catalogue that copies this record
        // should know not to treat it as a lasting dataset (PF-19).
        record["dct:accrualPeriodicity"] = json!("http://purl.org/cld/freq/irregular");
        record["adms:status"] = json!("http://purl.org/adms/status/UnderDevelopment");
    }
    record
}

/// One child of a space: the path the router serves, and what the record calls it.
struct Child {
    /// The path under `/cs/{space}/`, exactly as the router spells it.
    path: &'static str,
    /// The title all three serializations give it.
    title: &'static str,
    /// The specification it implements, for `dct:conformsTo`.
    conforms_to: Option<&'static str>,
}

/// The children of a space: every one the router serves, and nothing else (SP-04, T-2373).
///
/// A record is a promise a harvester follows. `schema/` was advertised as a directory nobody
/// routes, and `dump/` and `access` were linked from the HTML page alone; all three answered
/// 404. `dump/` is SP-04's and is not built (**T-2391**), `access` is not a child of a space
/// at all, and the schema surface is entered by its index — so those are what the record
/// says now. The one list below is what all three serializations render, so they cannot
/// drift apart again.
fn children(space: &Space) -> Vec<Child> {
    let mut children = vec![Child {
        path: "ngsi-ld/v1/",
        title: "NGSI-LD API",
        conforms_to: Some("https://www.etsi.org/deliver/etsi_gs/CIM/001_099/009/"),
    }];
    if space
        .endpoint
        .representations
        .contains(&jc_core::kinds::Representation::Mcp)
    {
        children.push(Child {
            path: "mcp",
            title: "Model Context Protocol",
            conforms_to: Some("https://modelcontextprotocol.io/specification"),
        });
    }
    children.push(Child {
        path: "schema/index.json",
        title: "Schema artifacts",
        conforms_to: None,
    });
    children
}

/// The data services and distributions of a space, which are exactly its children (SP-04).
fn services(space: &Space, iri: &str) -> Value {
    Value::Array(
        children(space)
            .into_iter()
            .map(|child| {
                let url = format!("{iri}/{}", child.path);
                let mut service = json!({
                    "@id": url,
                    "@type": "dcat:DataService",
                    "dct:title": child.title,
                    "dcat:endpointURL": url,
                    "dcat:servesDataset": iri,
                });
                if let Some(standard) = child.conforms_to {
                    service["dct:conformsTo"] = json!(standard);
                }
                service
            })
            .collect::<Vec<_>>(),
    )
}

/// The catalog of the spaces a caller may discover (SP-11).
///
/// The narrowing happened before this was called: what arrives here is already only what
/// the caller's grants reach, so the document has nothing left to hide.
pub fn catalog(spaces: &[std::sync::Arc<Space>], base: &str) -> Value {
    json!({
        "@context": "https://www.w3.org/ns/dcat.jsonld",
        "@id": format!("{base}/cs"),
        "@type": "dcat:Catalog",
        "dct:title": "joinedcontext context spaces",
        "dcat:dataset": spaces
            .iter()
            .map(|space| json!({
                "@id": format!("{base}/cs/{}", space.name()),
                "@type": "dcat:Dataset",
                "dct:identifier": space.name(),
                "dct:title": localized(&space.title, space.name()),
            }))
            .collect::<Vec<_>>(),
    })
}

/// The same dataset record as Turtle, for a triple store or a partner's connector.
pub fn dataset_turtle(space: &Space, base: &str) -> String {
    let iri = format!("{base}/cs/{}", space.name());
    let mut out = String::from(
        "@prefix dcat: <http://www.w3.org/ns/dcat#> .\n\
         @prefix dct: <http://purl.org/dc/terms/> .\n\n",
    );
    out.push_str(&format!(
        "<{iri}> a dcat:Dataset ;\n    dct:identifier {} ;\n    dct:title {} ",
        literal(space.name()),
        literal(&plain(&space.title, space.name()))
    ));
    if !space.description.is_empty() {
        out.push_str(&format!(
            ";\n    dct:description {} ",
            literal(&plain(&space.description, ""))
        ));
    }
    // The same children the JSON-LD record names: a triple store that harvests the Turtle
    // and a client that reads the JSON-LD have to learn the same surface (SP-10).
    let children = children(space);
    let links: Vec<String> = children
        .iter()
        .map(|child| format!("<{iri}/{}>", child.path))
        .collect();
    out.push_str(&format!(";\n    dcat:service {} .\n", links.join(", ")));
    for child in &children {
        out.push_str(&format!(
            "\n<{iri}/{path}> a dcat:DataService ;\n\
             \x20   dct:title {title} ;\n",
            path = child.path,
            title = literal(child.title)
        ));
        if let Some(standard) = child.conforms_to {
            out.push_str(&format!("    dct:conformsTo <{standard}> ;\n"));
        }
        out.push_str(&format!(
            "    dcat:endpointURL <{iri}/{}> ;\n    dcat:servesDataset <{iri}> .\n",
            child.path
        ));
    }
    out
}

/// The record as a page, for the browser that followed the URL (SP-10).
pub fn dataset_html(space: &Space, base: &str) -> String {
    let iri = format!("{base}/cs/{}", space.name());
    let title = escape(&plain(&space.title, space.name()));
    let description = escape(&plain(&space.description, ""));
    let children = children(space)
        .into_iter()
        .map(|child| {
            format!(
                "<li><a href=\"{iri}/{path}\">{title}</a> <code>{path}</code></li>",
                path = child.path,
                title = escape(child.title)
            )
        })
        .collect::<Vec<_>>()
        .join("");
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <title>{title}</title></head><body><h1>{title}</h1><p>{description}</p>\
         <p>Context space <code>{}</code>.</p><ul>{children}</ul>\
         <p><a href=\"{iri}\" type=\"application/ld+json\">DCAT-AP record</a></p>\
         </body></html>",
        escape(space.name())
    )
}

/// A language map as JSON-LD wants it: one `@value`/`@language` object per locale, or a
/// plain string for text of no locale (a plain manifest title, or none at all).
pub(crate) fn localized(map: &BTreeMap<String, String>, fallback: &str) -> Value {
    if map.is_empty() {
        return json!(fallback);
    }
    if let (1, Some(text)) = (map.len(), map.get("")) {
        return json!(text);
    }
    Value::Array(
        map.iter()
            .map(|(locale, text)| match locale.as_str() {
                "" => json!(text),
                locale => json!({ "@value": text, "@language": locale }),
            })
            .collect(),
    )
}

/// One string out of a language map, for the serializations that carry no language tag.
pub(crate) fn plain(map: &BTreeMap<String, String>, fallback: &str) -> String {
    map.get("en")
        .or_else(|| map.values().next())
        .cloned()
        .unwrap_or_else(|| fallback.to_owned())
}

/// A Turtle string literal, with the four escapes the grammar requires.
pub(crate) fn literal(text: &str) -> String {
    let escaped = text
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r");
    format!("\"{escaped}\"")
}

/// HTML text: the manifest wrote the title, so it is escaped before it is rendered.
pub(crate) fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accept_picks_the_representation_the_client_can_read() {
        assert_eq!(negotiate(None), Format::JsonLd);
        assert_eq!(negotiate(Some("*/*")), Format::JsonLd);
        assert_eq!(negotiate(Some("text/turtle")), Format::Turtle);
        assert_eq!(
            negotiate(Some("text/html,application/xhtml+xml,*/*;q=0.8")),
            Format::Html
        );
        assert_eq!(negotiate(Some("application/ld+json")), Format::JsonLd);
    }

    #[test]
    fn a_plain_title_carries_no_language_tag() {
        let plain_title = BTreeMap::from([(String::new(), "Air quality".to_owned())]);
        assert_eq!(localized(&plain_title, "air"), json!("Air quality"));
        assert_eq!(plain(&plain_title, "air"), "Air quality");
        let legacy = BTreeMap::from([("fi".to_owned(), "Ilmanlaatu".to_owned())]);
        assert_eq!(
            localized(&legacy, "air"),
            json!([{ "@value": "Ilmanlaatu", "@language": "fi" }])
        );
        assert_eq!(localized(&BTreeMap::new(), "air"), json!("air"));
    }

    #[test]
    fn a_title_from_a_manifest_cannot_close_the_page_it_is_rendered_in() {
        assert_eq!(
            escape("<script>x</script>"),
            "&lt;script&gt;x&lt;/script&gt;"
        );
        assert_eq!(literal("say \"hi\"\n"), "\"say \\\"hi\\\"\\n\"");
    }
}
