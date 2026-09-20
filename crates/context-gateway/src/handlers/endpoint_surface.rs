//! The DCAT-AP record an Endpoint answers with at its own root (T-0337, T-0338, EP-27,
//! EP-68, EP-69).
//!
//! `GET /api/endpoint/{slug}/` is the endpoint's front door, and it is the one document a
//! catalogue, a data space connector and a person all read: the CKAN publisher takes its
//! metadata from here rather than authoring any (EP-63), so the catalogue entry and the
//! endpoint cannot disagree.
//!
//! Two families of distribution are listed. The representations are what the endpoint
//! serves of the data; the schema artifacts are what it serves of the model, each with the
//! sha256 of the document *this caller* would download. The schema surface projects to the
//! grant, so two callers see two records and each names its own digests — which is also
//! the artifact's `ETag`, so a harvester can tell a stale copy without fetching it (EP-51).
//!
//! Everything here is the granted projection. A representation or an artifact the caller
//! may not read is absent rather than listed and then refused, because listing it would
//! disclose that it exists (R20).

use crate::handlers::space_surface::{escape, literal, localized, plain};
use crate::resolver::{Endpoint, Space};
use jc_core::kinds::{Audience, Representation};
use serde_json::{json, Value};
use std::collections::BTreeMap;

/// What one representation is, as a catalogue needs to describe it (EP-05, EP-68).
struct Served {
    /// The path under the endpoint root.
    path: &'static str,
    /// The title a person reads in the catalogue.
    title: &'static str,
    /// The media type the distribution answers with.
    media_type: &'static str,
    /// The standard it conforms to, or nothing for a plain file format.
    conforms_to: Option<&'static str>,
}

/// The catalogue description of every representation an endpoint may serve.
///
/// The media type is what the gateway actually answers with, not what the extension
/// suggests, so a harvester that content-negotiates on this record gets what it asked for.
fn served(representation: Representation) -> Served {
    match representation {
        Representation::NgsiLd => Served {
            path: "ngsi-ld/v1/",
            title: "NGSI-LD API",
            media_type: "application/ld+json",
            conforms_to: Some("https://www.etsi.org/deliver/etsi_gs/CIM/001_099/009/"),
        },
        Representation::Mcp => Served {
            path: "mcp",
            title: "Model Context Protocol",
            media_type: "application/json",
            conforms_to: Some("https://modelcontextprotocol.io/specification"),
        },
        Representation::GeoJson => Served {
            path: "file.geojson",
            title: "GeoJSON",
            media_type: "application/geo+json",
            conforms_to: Some("https://www.rfc-editor.org/rfc/rfc7946"),
        },
        Representation::Csv => Served {
            path: "file.csv",
            title: "CSV",
            media_type: "text/csv",
            conforms_to: None,
        },
        Representation::Xlsx => Served {
            path: "file.xlsx",
            title: "Excel workbook",
            media_type: "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            conforms_to: None,
        },
        Representation::Json => Served {
            path: "file.json",
            title: "JSON",
            media_type: "application/json",
            conforms_to: None,
        },
        Representation::Zip => Served {
            path: "file.zip",
            title: "Bundle of every representation",
            media_type: "application/zip",
            conforms_to: None,
        },
        Representation::OgcFeatures => Served {
            path: "ogc/features/",
            title: "OGC API - Features",
            media_type: "application/geo+json",
            conforms_to: Some("http://www.opengis.net/spec/ogcapi-features-1/1.0/conf/core"),
        },
        Representation::Sta => Served {
            path: "sta/v1.1/",
            title: "SensorThings API",
            media_type: "application/json",
            conforms_to: Some("http://www.opengis.net/spec/sensorthings/1.1"),
        },
    }
}

/// The formalism a schema artifact is written in, for `dcterms:conformsTo` (EP-46, EP-68).
///
/// A file name rather than the `Artifact` enum: the descriptors arrive as the JSON the
/// schema index already built, and rebuilding them from the enum would compute every
/// digest a second time.
fn formalism(file_name: &str) -> Option<&'static str> {
    match file_name {
        "model.linkml.yaml" => Some("https://w3id.org/linkml/"),
        "model.schema.json" => Some("http://json-schema.org/draft-07/schema#"),
        "context.jsonld" => Some("https://www.w3.org/TR/json-ld11/"),
        "model.shacl.ttl" => Some("https://www.w3.org/TR/shacl/"),
        "model.owl.ttl" => Some("https://www.w3.org/TR/owl2-overview/"),
        "model.rdf.ttl" => Some("https://www.w3.org/TR/rdf11-concepts/"),
        "model.md" => Some("https://commonmark.org/"),
        _ => None,
    }
}

/// `PUBLIC` or `RESTRICTED`, from the EU authority table (EP-69).
///
/// A catalogue reads this before it harvests, which is why it is on the record rather than
/// only in the policy set: a dataset anybody may open and one that needs a token are
/// different things to list.
fn access_rights(audience: Audience) -> &'static str {
    match audience {
        Audience::Public => "http://publications.europa.eu/resource/authority/access-right/PUBLIC",
        _ => "http://publications.europa.eu/resource/authority/access-right/RESTRICTED",
    }
}

/// The IRI of the endpoint, which is what every distribution hangs off.
fn iri(endpoint: &Endpoint, base: &str) -> String {
    format!("{base}{}", endpoint.base_path)
}

/// One distribution per representation the endpoint serves (EP-05, EP-68).
fn representation_distributions(endpoint: &Endpoint, iri: &str) -> Vec<Value> {
    let mut out = Vec::new();
    for representation in &endpoint.representations {
        let served = served(*representation);
        let url = format!("{iri}/{}", served.path);
        let mut distribution = json!({
            "@id": url,
            "@type": "dcat:Distribution",
            "dct:title": served.title,
            "dct:format": representation.as_str(),
            "dcat:accessURL": url,
            "dcat:mediaType": served.media_type,
        });
        if let Some(standard) = served.conforms_to {
            distribution["dct:conformsTo"] = json!(standard);
        }
        out.push(distribution);
    }
    out
}

/// One distribution per schema artifact, with the digest of the projected document (EP-68).
///
/// `index` is the schema catalogue the endpoint already serves, so the digests here and the
/// ones under `schema/index.json` are the same numbers computed once.
fn schema_distributions(index: &Value, iri: &str) -> Vec<Value> {
    let mut out = Vec::new();
    let Some(models) = index.get("models").and_then(Value::as_array) else {
        return out;
    };
    for model in models {
        let Some(major) = model.get("version").and_then(Value::as_u64) else {
            continue;
        };
        let Some(artifacts) = model.get("artifacts").and_then(Value::as_object) else {
            continue;
        };
        for (file_name, descriptor) in artifacts {
            let url = format!("{iri}/schema/v{major}/{file_name}");
            let mut distribution = json!({
                "@id": url,
                "@type": "dcat:Distribution",
                "dct:title": format!("{} {file_name}", model.get("name").and_then(Value::as_str).unwrap_or_default()),
                "dcat:accessURL": url,
            });
            if let Some(media_type) = descriptor.get("type") {
                distribution["dcat:mediaType"] = media_type.clone();
            }
            if let Some(bytes) = descriptor.get("bytes") {
                distribution["dcat:byteSize"] = bytes.clone();
            }
            if let Some(sha256) = descriptor.get("sha256").and_then(Value::as_str) {
                distribution["spdx:checksum"] = json!({
                    "@type": "spdx:Checksum",
                    "spdx:algorithm": "spdx:checksumAlgorithm_sha256",
                    "spdx:checksumValue": sha256,
                });
            }
            if let Some(standard) = formalism(file_name) {
                distribution["dct:conformsTo"] = json!(standard);
            }
            out.push(distribution);
        }
    }
    out
}

/// Every formalism the record names, for `dcterms:conformsTo` on the dataset itself.
fn conforms_to(schema: &[Value]) -> Vec<Value> {
    let mut standards: Vec<Value> = Vec::new();
    for distribution in schema {
        if let Some(standard) = distribution.get("dct:conformsTo") {
            if !standards.contains(standard) {
                standards.push(standard.clone());
            }
        }
    }
    standards
}

/// The title and description a person reads: the endpoint's own `metadata`, else the
/// space's (EP-27).
///
/// The endpoint's text comes first because several endpoints publish slices of one space
/// (EP-14, GW8): a catalogue that harvested the space's title for every one of them would
/// list the same dataset name four times. An endpoint whose manifest names no text, and one
/// whose space is not resolvable, fall back to the space name, which is what the URL
/// already says.
fn texts<'a>(
    endpoint: &'a Endpoint,
    space: Option<&'a Space>,
) -> (&'a BTreeMap<String, String>, &'a BTreeMap<String, String>) {
    let title = if endpoint.title.is_empty() {
        space.map(|s| &s.title).unwrap_or(&EMPTY)
    } else {
        &endpoint.title
    };
    let description = if endpoint.description.is_empty() {
        space.map(|s| &s.description).unwrap_or(&EMPTY)
    } else {
        &endpoint.description
    };
    (title, description)
}

/// The DCAT-AP dataset record of one endpoint (EP-27, EP-68, EP-69).
pub fn dataset(endpoint: &Endpoint, space: Option<&Space>, index: &Value, base: &str) -> Value {
    let iri = iri(endpoint, base);
    let (title, description) = texts(endpoint, space);
    let schema = schema_distributions(index, &iri);

    let mut distributions = representation_distributions(endpoint, &iri);
    distributions.extend(schema.iter().cloned());

    let mut record = json!({
        "@context": "https://www.w3.org/ns/dcat.jsonld",
        "@id": iri,
        "@type": "dcat:Dataset",
        "dct:identifier": endpoint.slug,
        "dct:title": localized(title, &endpoint.space),
        "dct:accessRights": access_rights(endpoint.audience),
        "dct:conformsTo": conforms_to(&schema),
        "dcat:distribution": distributions,
    });
    if !description.is_empty() {
        record["dct:description"] = localized(description, "");
    }
    if endpoint.audience != Audience::Public {
        // What a connector dereferences to build an offer: the endpoint's own grant
        // document, which is the machine-readable form of the policy in force (DS-08).
        record["odrl:hasPolicy"] = json!(format!("{iri}/access"));
    }
    record
}

/// An empty language map, so a space that resolved to nothing takes the same path as one
/// whose manifest named no title.
static EMPTY: BTreeMap<String, String> = BTreeMap::new();

/// The same record as Turtle, for a triple store or a partner's connector (EP-27).
pub fn dataset_turtle(
    endpoint: &Endpoint,
    space: Option<&Space>,
    index: &Value,
    base: &str,
) -> String {
    let iri = iri(endpoint, base);
    let title = plain(texts(endpoint, space).0, &endpoint.space);
    let mut out = String::from(
        "@prefix dcat: <http://www.w3.org/ns/dcat#> .\n\
         @prefix dct: <http://purl.org/dc/terms/> .\n\
         @prefix odrl: <http://www.w3.org/ns/odrl/2/> .\n\
         @prefix spdx: <http://spdx.org/rdf/terms#> .\n\n",
    );
    out.push_str(&format!(
        "<{iri}> a dcat:Dataset ;\n    dct:identifier {} ;\n    dct:title {} ;\n    dct:accessRights <{}> ",
        literal(&endpoint.slug),
        literal(&title),
        access_rights(endpoint.audience),
    ));
    if endpoint.audience != Audience::Public {
        out.push_str(&format!(";\n    odrl:hasPolicy <{iri}/access> "));
    }

    let mut distributions = representation_distributions(endpoint, &iri);
    distributions.extend(schema_distributions(index, &iri));
    for distribution in &distributions {
        if let Some(url) = distribution.get("dcat:accessURL").and_then(Value::as_str) {
            out.push_str(&format!(";\n    dcat:distribution <{url}> "));
        }
    }
    out.push_str(".\n\n");

    for distribution in &distributions {
        let Some(url) = distribution.get("dcat:accessURL").and_then(Value::as_str) else {
            continue;
        };
        out.push_str(&format!(
            "<{url}> a dcat:Distribution ;\n    dcat:accessURL <{url}> "
        ));
        if let Some(media_type) = distribution.get("dcat:mediaType").and_then(Value::as_str) {
            out.push_str(&format!(";\n    dcat:mediaType {} ", literal(media_type)));
        }
        if let Some(standard) = distribution.get("dct:conformsTo").and_then(Value::as_str) {
            out.push_str(&format!(";\n    dct:conformsTo <{standard}> "));
        }
        if let Some(sha256) = distribution
            .get("spdx:checksum")
            .and_then(|checksum| checksum.get("spdx:checksumValue"))
            .and_then(Value::as_str)
        {
            out.push_str(&format!(";\n    spdx:checksum {} ", literal(sha256)));
        }
        out.push_str(".\n");
    }
    out
}

/// The record as a page, for the person who followed the URL (EP-27).
pub fn dataset_html(
    endpoint: &Endpoint,
    space: Option<&Space>,
    index: &Value,
    base: &str,
) -> String {
    let iri = iri(endpoint, base);
    let (title, description) = texts(endpoint, space);
    let title = escape(&plain(title, &endpoint.space));
    let description = escape(&plain(description, ""));
    let rights = if endpoint.audience == Audience::Public {
        "public"
    } else {
        "restricted"
    };

    let mut distributions = representation_distributions(endpoint, &iri);
    distributions.extend(schema_distributions(index, &iri));
    let items = distributions
        .iter()
        .filter_map(|distribution| {
            let url = distribution.get("dcat:accessURL").and_then(Value::as_str)?;
            let label = distribution
                .get("dct:title")
                .and_then(Value::as_str)
                .unwrap_or(url);
            Some(format!(
                "<li><a href=\"{}\">{}</a></li>",
                escape(url),
                escape(label)
            ))
        })
        .collect::<Vec<_>>()
        .join("");

    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <title>{title}</title></head><body><h1>{title}</h1><p>{description}</p>\
         <p>Endpoint of context space <code>{}</code>, access {rights}.</p><ul>{items}</ul>\
         <p><a href=\"{iri}/\" type=\"application/ld+json\">DCAT-AP record</a></p>\
         </body></html>",
        escape(&endpoint.space)
    )
}
