//! The DCAT-AP record of an Endpoint (T-0337, T-0338, EP-27, EP-68, EP-69), written once and
//! served by the gateway at the Endpoint's root (ADR-N-051, T-4499).
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
//! Everything here is the granted projection: the gateway hands in a [`Subject`] of what the
//! caller may read. A representation or an artifact the caller may not read is absent rather
//! than listed and then refused, because listing it would disclose that it exists (R20).

pub mod profile;

use jc_core::classification::classify;
use jc_core::kinds::catalog::spatial_iri;
use jc_core::kinds::{
    feed_links, ical_links, Audience, Catalog, ContactPoint, Duty, FeedOutput, IcalOutput,
    ProjectCatalog, Representation,
};
use serde_json::{json, Value};
use std::collections::BTreeMap;

/// What the record is written from: the Endpoint as the gateway resolved it, and its space's
/// texts, which stand in for an Endpoint that names no title or description.
#[derive(Debug, Clone, Copy)]
pub struct Subject<'a> {
    /// The Endpoint's slug, the record's `dct:identifier`.
    pub slug: &'a str,
    /// The context space it serves.
    pub space: &'a str,
    /// Its path under the public base URL: `/api/endpoint/{slug}` or `/cs/{space}`.
    pub base_path: &'a str,
    /// The Endpoint's title per locale.
    pub title: &'a BTreeMap<String, String>,
    /// The Endpoint's description per locale.
    pub description: &'a BTreeMap<String, String>,
    /// The space's title per locale, when the space resolved.
    pub space_title: Option<&'a BTreeMap<String, String>>,
    /// The space's description per locale, when the space resolved.
    pub space_description: Option<&'a BTreeMap<String, String>>,
    /// Who may read it.
    pub audience: Audience,
    /// The representations it serves.
    pub representations: &'a [Representation],
    /// Its catalogue block, when it has one.
    pub catalog: Option<&'a Catalog>,
    /// The entity types it holds: the space's declared classes, else its models' classes.
    pub classes: &'a [&'a str],
    /// The `assigner` of every policy in force on it.
    pub assigners: &'a [&'a str],
    /// Its feed outputs, each one distribution per format (EP-150, EP-151).
    pub feeds: &'a [FeedOutput],
    /// Its iCalendar outputs, each one distribution (API/11 EP-159); empty with `ical` served:
    /// the one calendar at `ical/v1/events.ics`.
    pub calendars: &'a [IcalOutput],
}

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
            // What a plain GET answers: NGSI-LD sends `application/json` with the `@context` in
            // the Link header unless Accept asks for ld+json (CIM 009 6.3.6, T-4539).
            media_type: "application/json",
            conforms_to: Some("https://www.etsi.org/deliver/etsi_gs/CIM/001_099/009/"),
        },
        Representation::Mcp => Served {
            path: "mcp",
            title: "Model Context Protocol",
            media_type: "application/json",
            conforms_to: Some("https://modelcontextprotocol.io/specification"),
        },
        Representation::Csv => Served {
            path: "file.csv",
            title: "CSV",
            media_type: "text/csv",
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
        Representation::Odata => Served {
            path: "odata/v4/",
            title: "OData v4",
            media_type: "application/json",
            conforms_to: Some("https://docs.oasis-open.org/odata/odata/v4.01/"),
        },
        Representation::WebSocket => Served {
            path: "ws",
            title: "Live updates over WebSocket",
            media_type: "application/json",
            conforms_to: None,
        },
        Representation::Ical => Served {
            path: "ical/v1/events.ics",
            title: "iCalendar feed",
            media_type: "text/calendar",
            conforms_to: Some("https://www.rfc-editor.org/rfc/rfc5545"),
        },
        // Listed per output and format by `feed_distributions`; this is the surface's root.
        Representation::Feed => Served {
            path: "feed/v1/",
            title: "Feeds",
            media_type: "application/atom+xml",
            conforms_to: Some("https://www.rfc-editor.org/rfc/rfc4287"),
        },
    }
}

/// The standard and EU file-type code of one feed format (API/02 §8b).
fn feed_format(format: &str) -> (&'static str, &'static str) {
    match format {
        "RSS 2.0" => ("https://www.rssboard.org/rss-specification", "RSS"),
        "JSON Feed" => ("https://www.jsonfeed.org/version/1.1/", "JSON"),
        _ => ("https://www.rfc-editor.org/rfc/rfc4287", "ATOM"),
    }
}

/// One distribution per iCalendar output, titled by the output (API/11 EP-159).
fn calendar_distributions(endpoint: &Subject<'_>, iri: &str) -> Vec<Value> {
    let served = served(Representation::Ical);
    ical_links(endpoint.calendars)
        .into_iter()
        .map(|(output, path)| {
            let url = format!("{iri}/{path}");
            let mut distribution = json!({
                "@id": url,
                "@type": "dcat:Distribution",
                "dct:title": localized(&output.title, &format!("{} ({})", output.name, served.title)),
                "dct:format": file_type_iri(Representation::Ical),
                "dcat:accessURL": url,
                "dcat:mediaType": media_type_iri(served.media_type),
                "dcat:accessService": service_iri(iri),
                "dct:conformsTo": served.conforms_to,
            });
            if !output.description.is_empty() {
                distribution["dct:description"] = json!(language_values(&output.description));
            }
            licensed(&mut distribution, endpoint.catalog);
            distribution
        })
        .collect()
}

/// One distribution per feed output and format (EP-150, EP-151).
fn feed_distributions(endpoint: &Subject<'_>, iri: &str) -> Vec<Value> {
    feed_links(endpoint.feeds)
        .into_iter()
        .map(|(output, path, format, media_type)| {
            let url = format!("{iri}/{path}");
            let (standard, code) = feed_format(format);
            let titled: BTreeMap<String, String> = output
                .title
                .iter()
                .map(|(lang, text)| (lang.clone(), format!("{text} ({format})")))
                .collect();
            let mut distribution = json!({
                "@id": url,
                "@type": "dcat:Distribution",
                "dct:title": localized(&titled, &format!("{} ({format})", output.name)),
                "dct:format": format!("http://publications.europa.eu/resource/authority/file-type/{code}"),
                "dcat:accessURL": url,
                "dcat:mediaType": media_type_iri(media_type),
                "dcat:accessService": service_iri(iri),
                "dct:conformsTo": standard,
            });
            if !output.description.is_empty() {
                distribution["dct:description"] = json!(language_values(&output.description));
            }
            licensed(&mut distribution, endpoint.catalog);
            distribution
        })
        .collect()
}

/// The path of a representation under the Endpoint's root, as the record lists it.
pub fn path_of(representation: Representation) -> &'static str {
    served(representation).path
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
        // A published Mapping's three files (EP-53).
        name if name.ends_with(".linkml-map.yaml") => Some("https://w3id.org/linkml/mapper/"),
        name if name.ends_with(".blobl") => {
            Some("https://warpstreamlabs.github.io/bento/docs/guides/bloblang/about")
        }
        name if name.ends_with(".ir.json") => Some("https://joinedcontext.com/ns/mapping-ir"),
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
fn iri(endpoint: &Subject<'_>, base: &str) -> String {
    format!("{base}{}", endpoint.base_path)
}

/// The prefixes the record's compact keys use, in the order Turtle declares them.
const PREFIXES: [(&str, &str); 15] = [
    ("dcat", "http://www.w3.org/ns/dcat#"),
    ("dct", "http://purl.org/dc/terms/"),
    ("dcatap", "http://data.europa.eu/r5r/"),
    ("foaf", "http://xmlns.com/foaf/0.1/"),
    ("vcard", "http://www.w3.org/2006/vcard/ns#"),
    ("odrl", "http://www.w3.org/ns/odrl/2/"),
    ("ngsi-ld", "https://joinedcontext.com/odrl/ngsi-ld/v1#"),
    ("prov", "http://www.w3.org/ns/prov#"),
    ("skos", "http://www.w3.org/2004/02/skos/core#"),
    ("spdx", "http://spdx.org/rdf/terms#"),
    ("eli", "http://data.europa.eu/eli/ontology#"),
    ("rdfs", "http://www.w3.org/2000/01/rdf-schema#"),
    ("xsd", "http://www.w3.org/2001/XMLSchema#"),
    // the national terms-of-use vocabularies (API/08 sections 2 and 3, T-4499)
    ("leg", "https://data.gov.sk/def/ontology/legislation/"),
    ("pu", "https://data.gov.cz/slovník/podmínky-užití/"),
];

/// Keys whose string values are IRIs, not text (EP-78).
///
/// The record keeps compact keys and plain strings, which is what a reader that reads keys
/// expects; the `@context` built from this list is what makes an RDF reader see IRIs.
const IRI_KEYS: [&str; 26] = [
    "dct:accessRights",
    "foaf:page",
    "foaf:homepage",
    "dct:conformsTo",
    "dct:format",
    "dct:license",
    "dct:spatial",
    "dct:accrualPeriodicity",
    "dct:source",
    "dcat:accessURL",
    "dcat:downloadURL",
    "dcat:mediaType",
    "dcat:theme",
    "dcat:endpointURL",
    "dcat:endpointDescription",
    "dcat:servesDataset",
    "dcat:accessService",
    "dcatap:applicableLegislation",
    "odrl:hasPolicy",
    "odrl:action",
    "odrl:target",
    "odrl:assigner",
    "odrl:leftOperand",
    "odrl:operator",
    "vcard:hasEmail",
    "spdx:algorithm",
];

/// Keys whose string values are typed literals, with their datatype.
const TYPED_KEYS: [(&str, &str); 4] = [
    ("dcat:byteSize", "xsd:nonNegativeInteger"),
    ("spdx:checksumValue", "xsd:hexBinary"),
    ("dcat:startDate", "xsd:date"),
    ("dcat:endDate", "xsd:date"),
];

/// The record's own `@context`: the prefixes, then the coercions (EP-78).
fn context() -> Value {
    let mut context = serde_json::Map::new();
    for (prefix, namespace) in PREFIXES {
        context.insert(prefix.to_owned(), json!(namespace));
    }
    for key in IRI_KEYS {
        context.insert(key.to_owned(), json!({ "@type": "@id" }));
    }
    for (key, datatype) in TYPED_KEYS {
        context.insert(key.to_owned(), json!({ "@type": datatype }));
    }
    Value::Object(context)
}

/// The IANA IRI of a media type, which DCAT-AP ranges `dcat:mediaType` over.
fn media_type_iri(media_type: &str) -> String {
    let bare = media_type.split(';').next().unwrap_or(media_type).trim();
    format!("https://www.iana.org/assignments/media-types/{bare}")
}

/// The EU file-type IRI of what a representation answers with, for `dct:format`.
fn file_type_iri(representation: Representation) -> String {
    let code = match representation {
        Representation::NgsiLd => "JSON_LD",
        Representation::OgcFeatures => "GEOJSON",
        Representation::Csv => "CSV",
        Representation::Ical => "ICS",
        Representation::Feed => "ATOM",
        Representation::Mcp
        | Representation::Sta
        | Representation::Odata
        | Representation::WebSocket => "JSON",
    };
    format!("http://publications.europa.eu/resource/authority/file-type/{code}")
}

/// The IRI of the endpoint as a `dcat:DataService`.
fn service_iri(iri: &str) -> String {
    format!("{iri}#service")
}

/// The IRI of the offer the licence makes (EP-79).
fn offer_iri(iri: &str) -> String {
    format!("{iri}#offer")
}

/// What every distribution carries of the catalogue block: the licence and the attribution.
fn licensed(distribution: &mut Value, catalog: Option<&Catalog>) {
    let Some(catalog) = catalog else { return };
    if let Some(licence) = catalog.license {
        distribution["dct:license"] = json!(licence.iri());
    }
    if !catalog.attribution.is_empty() {
        distribution["dct:rights"] = json!({
            "@type": "dct:RightsStatement",
            "rdfs:label": language_values(&catalog.attribution),
        });
    }
}

/// One distribution per representation the endpoint serves (EP-05, EP-68).
fn representation_distributions(endpoint: &Subject<'_>, iri: &str) -> Vec<Value> {
    let mut out = Vec::new();
    for representation in endpoint.representations {
        if *representation == Representation::Feed {
            out.extend(feed_distributions(endpoint, iri));
            continue;
        }
        if *representation == Representation::Ical && !endpoint.calendars.is_empty() {
            out.extend(calendar_distributions(endpoint, iri));
            continue;
        }
        let served = served(*representation);
        let url = format!("{iri}/{}", served.path);
        let mut distribution = json!({
            "@id": url,
            "@type": "dcat:Distribution",
            "dct:title": served.title,
            "dct:format": file_type_iri(*representation),
            "dcat:accessURL": url,
            "dcat:mediaType": media_type_iri(served.media_type),
            "dcat:accessService": service_iri(iri),
        });
        if let Some(standard) = served.conforms_to {
            distribution["dct:conformsTo"] = json!(standard);
        }
        if *representation == Representation::WebSocket {
            let base = iri.strip_suffix(endpoint.base_path).unwrap_or_default();
            explain_websocket(&mut distribution, endpoint, &url, base);
        }
        licensed(&mut distribution, endpoint.catalog);
        out.push(distribution);
    }
    out
}

/// What the WebSocket distribution says of itself (T-4064, API/07 §1): one sentence in the
/// Endpoint's languages, and the guide as its page and as the message format it conforms to.
const WEBSOCKET_SENTENCES: [(&str, &str); 3] = [
    ("en", "Open {address} with the subprotocol ngsi-ld-ws and send an x-subscribe message; each change arrives as a message."),
    ("fi", "Avaa {address} aliprotokollalla ngsi-ld-ws ja lähetä x-subscribe-viesti; jokainen muutos saapuu viestinä."),
    ("sk", "Otvorte {address} s podprotokolom ngsi-ld-ws a pošlite správu x-subscribe; každá zmena príde ako správa."),
];

fn explain_websocket(distribution: &mut Value, endpoint: &Subject<'_>, url: &str, base: &str) {
    let address = ws_address(url);
    let described: Vec<Value> = WEBSOCKET_SENTENCES
        .iter()
        .filter(|(language, _)| {
            *language == "en"
                || endpoint
                    .title
                    .get(*language)
                    .is_some_and(|title| !title.trim().is_empty())
        })
        .map(|(language, sentence)| {
            json!({ "@value": sentence.replace("{address}", &address), "@language": language })
        })
        .collect();
    let guide = format!("{base}/docs/API/07-websocket");
    distribution["dct:description"] = json!(described);
    distribution["foaf:page"] = json!(guide);
    distribution["dct:conformsTo"] = json!(guide);
}

/// One distribution per schema artifact, with the digest of the projected document (EP-68),
/// and one per file of every Mapping the caller may read (EP-53), from the index's `mappings`.
///
/// `index` is the schema catalogue the endpoint already serves, so the digests here and the
/// ones under `schema/index.json` are the same numbers computed once.
fn schema_distributions(endpoint: &Subject<'_>, index: &Value, iri: &str) -> Vec<Value> {
    let mut out = Vec::new();
    let listed = |key| {
        index
            .get(key)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
    };
    let models = listed("models").map(|model| (model, ""));
    let mappings = listed("mappings").map(|mapping| (mapping, "mappings/"));
    for (model, folder) in models.chain(mappings) {
        let Some(major) = major_of(model) else {
            continue;
        };
        let Some(artifacts) = model.get("artifacts").and_then(Value::as_object) else {
            continue;
        };
        for (file_name, descriptor) in artifacts {
            let url = format!("{iri}/schema/v{major}/{folder}{file_name}");
            let mut distribution = json!({
                "@id": url,
                "@type": "dcat:Distribution",
                "dct:title": format!("{} {file_name}", model.get("name").and_then(Value::as_str).unwrap_or_default()),
                "dcat:accessURL": url,
            });
            if let Some(media_type) = descriptor.get("type").and_then(Value::as_str) {
                distribution["dcat:mediaType"] = json!(media_type_iri(media_type));
            }
            if let Some(bytes) = descriptor.get("bytes").and_then(Value::as_u64) {
                distribution["dcat:byteSize"] = json!(bytes.to_string());
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
            licensed(&mut distribution, endpoint.catalog);
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

/// A language map as JSON-LD value objects, one per language.
fn language_values(map: &BTreeMap<String, String>) -> Vec<Value> {
    map.iter()
        .map(|(language, text)| json!({ "@value": text, "@language": language }))
        .collect()
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
    endpoint: &Subject<'a>,
) -> (&'a BTreeMap<String, String>, &'a BTreeMap<String, String>) {
    // A map of blank entries is no title either, so the space's stands in for it (T-2496).
    let blank = |map: &BTreeMap<String, String>| map.values().all(|text| text.trim().is_empty());
    let title = if blank(endpoint.title) {
        endpoint.space_title.unwrap_or(&EMPTY)
    } else {
        endpoint.title
    };
    let description = if blank(endpoint.description) {
        endpoint.space_description.unwrap_or(&EMPTY)
    } else {
        endpoint.description
    };
    (title, description)
}

/// The offer the licence makes, or `None` when the endpoint names no licence (EP-79).
///
/// A non-public endpoint's offer names the organization and the audience class, never a
/// project, a group or a person: the record is read by callers who hold none of them (R20).
fn offer(endpoint: &Subject<'_>, iri: &str) -> Option<Value> {
    let catalog = endpoint.catalog?;
    let licence = catalog.license?;
    let mut permission = json!({
        "odrl:action": "odrl:use",
        "odrl:target": iri,
    });
    let duties = duties(licence.duty());
    if !duties.is_empty() {
        permission["odrl:duty"] = Value::Array(duties);
    }
    let organization = offer_assigner(endpoint, catalog);
    if endpoint.audience != Audience::Public {
        let mut constraints = vec![json!({
            "odrl:leftOperand": "ngsi-ld:audience",
            "odrl:operator": "odrl:eq",
            "odrl:rightOperand": endpoint.audience.as_str(),
        })];
        if let Some(organization) = &organization {
            constraints.insert(
                0,
                json!({
                    "odrl:leftOperand": "odrl:recipient",
                    "odrl:operator": "odrl:isPartOf",
                    "odrl:rightOperand": { "@id": organization },
                }),
            );
        }
        permission["odrl:constraint"] = Value::Array(constraints);
    }
    let mut offer = json!({
        "@id": offer_iri(iri),
        "@type": ["odrl:Offer", "odrl:Policy"],
        "odrl:permission": permission,
    });
    if let Some(organization) = organization {
        offer["odrl:assigner"] = json!(organization);
    }
    Some(offer)
}

/// Who offers: the publisher's own IRI, else the one organization every policy names.
fn offer_assigner(endpoint: &Subject<'_>, catalog: &Catalog) -> Option<String> {
    if let Some(uri) = catalog.publisher.as_ref().and_then(|p| p.uri.clone()) {
        return Some(uri);
    }
    let mut assigners = endpoint.assigners.iter().copied();
    let first = assigners.next()?;
    assigners.all(|a| a == first).then(|| first.to_owned())
}

/// The ODRL duties of a licence, as the offer and the access surface both write them (EP-79).
pub fn duties(duty: Duty) -> Vec<Value> {
    match duty {
        Duty::None => Vec::new(),
        Duty::Attribute => vec![json!({ "odrl:action": "odrl:attribute" })],
        Duty::AttributeShareAlike => vec![
            json!({ "odrl:action": "odrl:attribute" }),
            json!({ "odrl:action": SHARE_ALIKE }),
        ],
    }
}

/// The Creative Commons share-alike requirement, as an ODRL duty action (EP-79).
pub const SHARE_ALIKE: &str = "http://creativecommons.org/ns#ShareAlike";

/// The Endpoint's catalogue block, its themes and keywords derived from the Smart Data Models of
/// the types it holds where the block sets none (T-4498): a theme or keyword set by hand wins.
/// The types are the space's declared classes, else its models' classes.
pub fn classified(endpoint: &Subject<'_>) -> Catalog {
    let mut catalog = endpoint.catalog.cloned().unwrap_or_default();
    if !catalog.themes.is_empty() && !catalog.keywords.is_empty() {
        return catalog;
    }
    let derived = classification(endpoint);
    if catalog.themes.is_empty() {
        catalog.themes = derived.themes.into_iter().collect();
    }
    if catalog.keywords.is_empty() {
        catalog.keywords = derived.keywords;
    }
    catalog
}

/// The Smart Data Model classification of the types an Endpoint holds: the space's declared
/// classes, else its models' classes (T-4498).
pub fn classification(endpoint: &Subject<'_>) -> jc_core::classification::Classification {
    classify(endpoint.classes.iter().copied())
}

/// A desk as a `vcard:Organization` (EP-80), a `vcard:Kind` stated for a validator that runs
/// without RDFS inference.
fn contact_point(contact: &ContactPoint) -> Value {
    json!({
        "@type": ["vcard:Organization", "vcard:Kind"],
        "vcard:fn": contact.name,
        "vcard:hasEmail": format!("mailto:{}", contact.email),
    })
}

/// What the catalogue block says about the dataset itself (EP-78).
fn describe(record: &mut Value, included: &mut Vec<Value>, catalog: &Catalog) {
    if let Some(publisher) = &catalog.publisher {
        let mut agent = json!({
            "@type": "foaf:Agent",
            "foaf:name": language_values(&publisher.name),
        });
        if let Some(uri) = &publisher.uri {
            agent["@id"] = json!(uri);
        }
        record["dct:publisher"] = agent;
    }
    if let Some(contact) = &catalog.contact_point {
        record["dcat:contactPoint"] = contact_point(contact);
    }
    if let Some(licence) = catalog.license {
        // DCAT-AP licenses distributions; the dataset carries it too, for a reader of keys.
        record["dct:license"] = json!(licence.iri());
        included.push(json!({
            "@id": licence.iri(),
            "@type": "dct:LicenseDocument",
            "rdfs:label": licence.title(),
        }));
    }
    if !catalog.themes.is_empty() {
        record["dcat:theme"] = json!(catalog.themes.iter().map(|t| t.iri()).collect::<Vec<_>>());
        for theme in &catalog.themes {
            included.push(json!({
                "@id": theme.iri(),
                "@type": "skos:Concept",
                "skos:prefLabel": { "@value": theme.label(), "@language": "en" },
            }));
        }
    }
    let keywords: Vec<Value> = catalog
        .keywords
        .iter()
        .flat_map(|(language, words)| {
            words
                .iter()
                .map(move |word| json!({ "@value": word, "@language": language }))
        })
        .collect();
    if !keywords.is_empty() {
        record["dcat:keyword"] = Value::Array(keywords);
    }
    if !catalog.spatial.is_empty() {
        let places: Vec<String> = catalog.spatial.iter().map(|s| spatial_iri(s)).collect();
        for place in &places {
            included.push(json!({ "@id": place, "@type": "dct:Location" }));
        }
        record["dct:spatial"] = json!(places);
    }
    if let Some(period) = &catalog.temporal {
        let mut node = json!({ "@type": "dct:PeriodOfTime" });
        if let Some(start) = period.start {
            node["dcat:startDate"] = json!(start.to_string());
        }
        if let Some(end) = period.end {
            node["dcat:endDate"] = json!(end.to_string());
        }
        record["dct:temporal"] = node;
    }
    if let Some(frequency) = catalog.frequency {
        record["dct:accrualPeriodicity"] = json!(frequency.iri());
        included.push(json!({ "@id": frequency.iri(), "@type": "dct:Frequency" }));
    }
    if !catalog.source.is_empty() {
        record["dct:source"] = json!(catalog.source.iter().map(|s| &s.url).collect::<Vec<_>>());
        for source in &catalog.source {
            included.push(json!({
                "@id": source.url,
                "@type": "dcat:Dataset",
                "dct:title": language_values(&source.title),
                "dct:description": language_values(&source.description),
            }));
        }
    }
    if let Some(pipeline) = &catalog.pipeline_ref {
        record["prov:wasGeneratedBy"] = json!({
            "@type": "prov:Activity",
            "rdfs:label": format!("pipeline {}", pipeline.name),
        });
    }
    if !catalog.applicable_legislation.is_empty() {
        record["dcatap:applicableLegislation"] = json!(catalog.applicable_legislation);
        for law in &catalog.applicable_legislation {
            included.push(json!({ "@id": law, "@type": "eli:LegalResource" }));
        }
    }
}

/// The classes DCAT-AP requires of the values the distributions name.
///
/// An access URL is an `rdfs:Resource` by the shapes' range; saying so keeps the record valid
/// for a validator that runs without RDFS entailment.
fn typed_values(distributions: &[Value], included: &mut Vec<Value>) {
    let mut seen = std::collections::BTreeSet::new();
    for distribution in distributions {
        if let Some(url) = distribution.get("dcat:accessURL").and_then(Value::as_str) {
            included.push(json!({ "@id": url, "@type": "rdfs:Resource" }));
        }
        for (key, class) in [
            ("dcat:mediaType", "dct:MediaType"),
            ("dct:format", "dct:MediaTypeOrExtent"),
            ("dct:conformsTo", "dct:Standard"),
        ] {
            if let Some(value) = distribution.get(key).and_then(Value::as_str) {
                if seen.insert((value.to_owned(), class)) {
                    included.push(json!({ "@id": value, "@type": class }));
                }
            }
        }
    }
}

/// The DCAT-AP dataset record of one endpoint (EP-27, EP-68, EP-69, EP-78, EP-79).
pub fn dataset(endpoint: &Subject<'_>, index: &Value, base: &str) -> Value {
    let iri = iri(endpoint, base);
    let (title, description) = texts(endpoint);
    let schema = schema_distributions(endpoint, index, &iri);

    let mut distributions = representation_distributions(endpoint, &iri);
    distributions.extend(schema.iter().cloned());
    // One access URL is one distribution, however many index entries name it (T-2496).
    let mut seen = std::collections::BTreeSet::new();
    distributions.retain(|distribution| {
        distribution
            .get("dcat:accessURL")
            .and_then(Value::as_str)
            .is_some_and(|url| seen.insert(url.to_owned()))
    });

    let title = localized(title, endpoint.space);
    let mut record = json!({
        "@context": context(),
        "@id": iri,
        "@type": "dcat:Dataset",
        "dct:identifier": endpoint.slug,
        "dct:title": title,
        "dct:accessRights": access_rights(endpoint.audience),
        "dct:conformsTo": conforms_to(&schema),
        "dcat:distribution": distributions,
    });
    // DCAT-AP makes a description mandatory; one that names nothing says which space it is.
    record["dct:description"] = if description.is_empty() {
        json!(format!(
            "Data of the context space {}, served by this endpoint.",
            endpoint.space
        ))
    } else {
        localized(description, "")
    };

    let mut included = vec![json!({
        "@id": access_rights(endpoint.audience),
        "@type": "dct:RightsStatement",
    })];
    describe(&mut record, &mut included, &classified(endpoint));
    let mut policies = Vec::new();
    if endpoint.audience != Audience::Public {
        // What a connector dereferences to build an offer: the endpoint's own grant
        // document, which is the machine-readable form of the policy in force (DS-08).
        policies.push(json!(format!("{iri}/access")));
    }
    if let Some(offer) = offer(endpoint, &iri) {
        policies.push(offer);
    }
    match policies.len() {
        0 => {}
        1 => record["odrl:hasPolicy"] = policies.remove(0),
        _ => record["odrl:hasPolicy"] = Value::Array(policies),
    }

    let mut service = json!({
        "@id": service_iri(&iri),
        "@type": "dcat:DataService",
        "dct:title": record["dct:title"].clone(),
        "dcat:endpointURL": format!("{iri}/"),
        // The OpenAPI document the Endpoint serves of its read surface (EP-99), which DCAT-AP-CZ
        // requires of every data service.
        "dcat:endpointDescription": format!("{iri}/openapi.json"),
        "dcat:servesDataset": iri,
        "dct:accessRights": access_rights(endpoint.audience),
    });
    if let Some(licence) = endpoint.catalog.and_then(|c| c.license) {
        service["dct:license"] = json!(licence.iri());
    }
    included.push(json!({ "@id": format!("{iri}/"), "@type": "rdfs:Resource" }));
    included.push(json!({ "@id": format!("{iri}/openapi.json"), "@type": "rdfs:Resource" }));
    included.push(service);
    if let Some(distributions) = record["dcat:distribution"].as_array() {
        typed_values(distributions, &mut included);
    }
    included.push(json!({
        "@id": "spdx:checksumAlgorithm_sha256",
        "@type": "spdx:ChecksumAlgorithm",
    }));
    record["@included"] = Value::Array(included);
    record
}

/// The organization's catalogue for harvesters, one `dcat:Catalog` of the given dataset records
/// (EP-84, Architecture/21 §8).
///
/// The caller hands in the records of [`dataset`] as the anonymous caller reads them, one per
/// public Endpoint; this only gathers them, so the feed says nothing a public read of each
/// record would not. Each record's typed nodes move to the catalogue's `@included`, once.
pub fn feed(records: Vec<Value>, base: &str, org_domain: &str) -> Value {
    catalogue(
        records,
        json!({
            "@id": format!("{base}/catalog"),
            "dct:title": format!("Open data of {org_domain}"),
            "dct:description": format!(
                "Every dataset {org_domain} serves to anyone, one record per public endpoint."
            ),
            "dct:publisher": {
                "@id": format!("https://{org_domain}/"),
                "@type": "foaf:Agent",
                "foaf:name": org_domain,
            },
        }),
    )
}

/// Who a publisher's catalogue belongs to: a Project that names its publisher (EP-127, docs
/// API/08 section 6).
#[derive(Debug, Clone, Copy)]
pub struct CatalogueOwner<'a> {
    /// The project's name, the last segment of the catalogue's IRI.
    pub project: &'a str,
    /// The project's title per locale, the catalogue's title.
    pub title: &'a BTreeMap<String, String>,
    /// The project's description per locale; its title stands in when it has none.
    pub description: &'a BTreeMap<String, String>,
    /// The publishing body's domain, its home page.
    pub domain: Option<&'a str>,
    /// The publisher and the desk.
    pub catalog: &'a ProjectCatalog,
}

/// One publisher's catalogue: the records of its project's public Endpoints, under its own
/// publisher, contact point and home page (EP-127). As [`feed`], it only gathers the records.
pub fn publisher_feed(records: Vec<Value>, base: &str, owner: &CatalogueOwner<'_>) -> Value {
    let publisher = &owner.catalog.publisher;
    let title = if owner.title.is_empty() {
        &publisher.name
    } else {
        owner.title
    };
    let description = if owner.description.is_empty() {
        title
    } else {
        owner.description
    };
    let mut agent = json!({
        "@type": "foaf:Agent",
        "foaf:name": language_values(&publisher.name),
    });
    if let Some(uri) = &publisher.uri {
        agent["@id"] = json!(uri);
    }
    let mut header = json!({
        "@id": format!("{base}/catalog/{}", owner.project),
        "dct:title": localized(title, owner.project),
        "dct:description": localized(description, owner.project),
        "dct:publisher": agent,
    });
    if let Some(contact) = &owner.catalog.contact_point {
        header["dcat:contactPoint"] = contact_point(contact);
    }
    let mut feed = catalogue(records, header);
    if let Some(domain) = owner.domain {
        let home = format!("https://{domain}/");
        feed["foaf:homepage"] = json!(home);
        if let Some(included) = feed["@included"].as_array_mut() {
            included.push(json!({ "@id": home, "@type": "foaf:Document" }));
        }
    }
    feed
}

/// A `dcat:Catalog` of `records` under `header`, its `@id`, texts and publisher: each record's
/// typed nodes move to the catalogue's `@included`, once.
fn catalogue(records: Vec<Value>, mut header: Value) -> Value {
    let mut included: Vec<Value> = Vec::new();
    // Each node once, by its text: a linear search per node took 22 s for 10,000 records.
    let mut seen = std::collections::HashSet::new();
    let mut datasets = Vec::new();
    for mut record in records {
        let Some(object) = record.as_object_mut() else {
            continue;
        };
        object.remove("@context");
        if let Some(Value::Array(nodes)) = object.remove("@included") {
            for node in nodes {
                if seen.insert(node.to_string()) {
                    included.push(node);
                }
            }
        }
        if let Some(id) = object.get("@id").cloned() {
            datasets.push(json!({ "@id": id }));
            included.push(record);
        }
    }
    header["@context"] = context();
    header["@type"] = json!("dcat:Catalog");
    header["dcat:dataset"] = Value::Array(datasets);
    header["@included"] = Value::Array(included);
    header
}

/// [`feed`] as Turtle: the same graph, written by the record's own writer (EP-84).
pub fn feed_turtle(feed: &Value) -> String {
    turtle(feed)
}

/// An empty language map, so a space that resolved to nothing takes the same path as one
/// whose manifest named no title.
static EMPTY: BTreeMap<String, String> = BTreeMap::new();

/// A record of [`dataset`]'s shape as Turtle, a profile's rendering included.
pub fn turtle(record: &Value) -> String {
    let mut out = String::new();
    for (prefix, namespace) in PREFIXES {
        out.push_str(&format!("@prefix {prefix}: <{namespace}> .\n"));
    }
    out.push('\n');
    let mut queue = vec![record.clone()];
    if let Some(included) = record.get("@included").and_then(Value::as_array) {
        queue.extend(included.iter().cloned());
    }
    let mut written = std::collections::BTreeSet::new();
    while !queue.is_empty() {
        let node = queue.remove(0);
        let Some(id) = node.get("@id").and_then(Value::as_str) else {
            continue;
        };
        let subject = expand(id);
        let body = properties(&node, 1, &mut queue);
        // A node named twice (a distribution listed by two index entries) is one statement.
        if body.is_empty() || !written.insert((subject.clone(), body.clone())) {
            continue;
        }
        out.push_str(&format!("<{}>{body} .\n\n", iri_ref(&subject)));
    }
    out
}

/// The predicate-object list of one node; nested nodes with an `@id` are queued as statements
/// of their own and referenced, nodes without one are written in place as blank nodes.
fn properties(node: &Value, depth: usize, queue: &mut Vec<Value>) -> String {
    let Some(object) = node.as_object() else {
        return String::new();
    };
    let indent = "    ".repeat(depth);
    let mut lines = Vec::new();
    if let Some(types) = object.get("@type") {
        for class in values(types) {
            if let Some(class) = class.as_str() {
                lines.push(format!("a {}", term(class)));
            }
        }
    }
    for (key, value) in object {
        let Some(predicate) = predicate(key) else {
            continue;
        };
        for item in values(value) {
            if let Some(object) = object_term(key, item, depth, queue) {
                lines.push(format!("{predicate} {object}"));
            }
        }
    }
    lines
        .iter()
        .enumerate()
        .map(|(i, line)| {
            if i == 0 {
                format!(" {line}")
            } else {
                format!(" ;\n{indent}{line}")
            }
        })
        .collect()
}

/// One object of a triple, in Turtle.
fn object_term(key: &str, value: &Value, depth: usize, queue: &mut Vec<Value>) -> Option<String> {
    match value {
        Value::String(text) if IRI_KEYS.contains(&key) => {
            Some(format!("<{}>", iri_ref(&expand(text))))
        }
        Value::String(text) => Some(match TYPED_KEYS.iter().find(|(k, _)| *k == key) {
            Some((_, datatype)) => format!("{}^^{datatype}", literal(text)),
            None => literal(text),
        }),
        Value::Number(number) => Some(literal(&number.to_string())),
        Value::Bool(flag) => Some(flag.to_string()),
        Value::Object(map) => {
            if let Some(text) = map.get("@value").and_then(Value::as_str) {
                if let Some(datatype) = map.get("@type").and_then(Value::as_str) {
                    return Some(format!("{}^^{}", literal(text), term(datatype)));
                }
                return Some(match map.get("@language").and_then(Value::as_str) {
                    Some(language) if is_language_tag(language) => {
                        format!("{}@{language}", literal(text))
                    }
                    _ => literal(text),
                });
            }
            if let Some(id) = map.get("@id").and_then(Value::as_str) {
                if map.len() > 1 {
                    queue.push(value.clone());
                }
                return Some(format!("<{}>", iri_ref(&expand(id))));
            }
            let inner = properties(value, depth + 1, queue);
            let indent = "    ".repeat(depth);
            Some(format!("[{inner}\n{indent}]"))
        }
        Value::Null | Value::Array(_) => None,
    }
}

/// A single value or each value of an array.
fn values(value: &Value) -> Vec<&Value> {
    match value {
        Value::Array(items) => items.iter().collect(),
        other => vec![other],
    }
}

/// A key the Turtle can write: a compact IRI over a declared prefix.
fn is_known(key: &str) -> bool {
    key.split_once(':').is_some_and(|(prefix, local)| {
        PREFIXES.iter().any(|(p, _)| *p == prefix) && is_local(local)
    })
}

/// A key as a Turtle predicate: the prefixed name when Turtle can write it so, the full IRI
/// when the prefix is declared but its local name is not one Turtle abbreviates (`pu:osobní-údaje`),
/// `None` for a key of no declared prefix and for JSON-LD's own keywords.
fn predicate(key: &str) -> Option<String> {
    if key.starts_with('@') {
        return None;
    }
    if is_known(key) {
        return Some(key.to_owned());
    }
    let (prefix, _) = key.split_once(':')?;
    PREFIXES
        .iter()
        .any(|(p, _)| *p == prefix)
        .then(|| format!("<{}>", iri_ref(&expand(key))))
}

/// What Turtle allows after the colon of a prefixed name, conservatively.
fn is_local(local: &str) -> bool {
    !local.is_empty() && local.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// A compact IRI over a declared prefix, expanded; anything else as it is.
fn expand(text: &str) -> String {
    if let Some((prefix, local)) = text.split_once(':') {
        if let Some((_, namespace)) = PREFIXES.iter().find(|(p, _)| *p == prefix) {
            return format!("{namespace}{local}");
        }
    }
    text.to_owned()
}

/// A class name: the prefixed name when Turtle can write it so, else the full IRI.
fn term(text: &str) -> String {
    if is_known(text) {
        text.to_owned()
    } else {
        format!("<{}>", iri_ref(&expand(text)))
    }
}

/// A BCP 47 tag as Turtle's LANGTAG accepts it.
fn is_language_tag(tag: &str) -> bool {
    let mut parts = tag.split('-');
    parts
        .next()
        .is_some_and(|first| !first.is_empty() && first.chars().all(|c| c.is_ascii_alphabetic()))
        && parts.all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_alphanumeric()))
}

/// `text` inside `<…>`: what Turtle's IRIREF forbids (controls, space, `<>"{}|^` and backslash,
/// backtick) is percent-encoded, so a name out of the schema index cannot close the IRI and write
/// a statement of its own (T-2496).
fn iri_ref(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if c <= ' ' || "<>\"{}|^`\\".contains(c) {
            let mut bytes = [0u8; 4];
            for byte in c.encode_utf8(&mut bytes).bytes() {
                out.push_str(&format!("%{byte:02X}"));
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// The record as a page, for the person who followed the URL (EP-27).
pub fn dataset_html(endpoint: &Subject<'_>, index: &Value, base: &str) -> String {
    let iri = iri(endpoint, base);
    let (title, description) = texts(endpoint);
    let title = escape(&plain(title, endpoint.space));
    let description = escape(&plain(description, ""));
    let rights = if endpoint.audience == Audience::Public {
        "public"
    } else {
        "restricted"
    };

    let mut distributions = representation_distributions(endpoint, &iri);
    distributions.extend(schema_distributions(endpoint, index, &iri));
    let items = distributions
        .iter()
        .filter_map(|distribution| {
            let url = distribution.get("dcat:accessURL").and_then(Value::as_str)?;
            let label = match distribution.get("dct:title") {
                Some(Value::String(title)) => title.as_str(),
                Some(Value::Array(titles)) => titles
                    .iter()
                    .find(|t| t["@language"] == "en")
                    .or_else(|| titles.first())
                    .and_then(|t| t["@value"].as_str())
                    .unwrap_or(url),
                _ => url,
            };
            // The WebSocket says how to use it, in English, beside its guide (T-4064).
            let note = distribution
                .get("dct:description")
                .and_then(Value::as_array)
                .and_then(|values| values.iter().find(|value| value["@language"] == "en"))
                .and_then(|value| value["@value"].as_str())
                .map(|note| format!("<br>{}", escape(note)))
                .unwrap_or_default();
            let page = distribution
                .get("foaf:page")
                .and_then(Value::as_str)
                .map(|page| format!(" <a href=\"{}\">Guide</a>", escape(page)))
                .unwrap_or_default();
            Some(format!(
                "<li><a href=\"{}\">{}</a>{note}{page}</li>",
                escape(url),
                escape(label)
            ))
        })
        .collect::<Vec<_>>()
        .join("");

    // Who publishes it and under what licence: the two things a person asks first (EP-78).
    let mut about = String::new();
    if let Some(catalog) = endpoint.catalog {
        if let Some(publisher) = &catalog.publisher {
            about.push_str(&format!(
                "<p>Published by {}.</p>",
                escape(&plain(&publisher.name, ""))
            ));
        }
        if let Some(licence) = catalog.license {
            about.push_str(&format!(
                "<p>Licence: <a href=\"{}\">{}</a>.</p>",
                escape(&licence.iri()),
                escape(licence.title())
            ));
        }
    }

    // A feed reader or a calendar finds each output from the page itself (EP-150, EP-159).
    let calendars = ical_links(endpoint.calendars)
        .into_iter()
        .map(|(output, path)| {
            format!(
                "<link rel=\"alternate\" type=\"text/calendar\" title=\"{}\" href=\"{iri}/{}\">",
                escape(output.label("en")),
                escape(&path)
            )
        });
    let alternates: String = feed_links(endpoint.feeds)
        .into_iter()
        .map(|(output, path, format, media_type)| {
            format!(
                "<link rel=\"alternate\" type=\"{media_type}\" title=\"{} ({format})\" href=\"{iri}/{}\">",
                escape(output.label("en")),
                escape(&path)
            )
        })
        .chain(calendars)
        .collect();
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <title>{title}</title>{alternates}</head><body><h1>{title}</h1><p>{description}</p>{about}\
         <p>Endpoint of context space <code>{}</code>, access {rights}.</p><ul>{items}</ul>\
         <p><a href=\"{iri}/\" type=\"application/ld+json\">DCAT-AP record</a></p>\
         </body></html>",
        escape(endpoint.space)
    )
}

/// A language map as JSON-LD wants it: one `@value`/`@language` object per locale, or a
/// plain string for text of no locale (a plain manifest title, or none at all).
pub fn localized(map: &BTreeMap<String, String>, fallback: &str) -> Value {
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
///
/// A blank entry counts as none, so `{en: ""}` falls back like an empty map (T-2496).
pub fn plain(map: &BTreeMap<String, String>, fallback: &str) -> String {
    let written = |text: &&String| !text.trim().is_empty();
    map.get("en")
        .filter(written)
        .or_else(|| map.values().find(written))
        .cloned()
        .unwrap_or_else(|| fallback.to_owned())
}

/// A Turtle string literal, with the four escapes the grammar requires.
pub fn literal(text: &str) -> String {
    let escaped = text
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r");
    format!("\"{escaped}\"")
}

/// HTML text: the manifest wrote the title, so it is escaped before it is rendered.
pub fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// The WebSocket address of an `http(s)` URL.
pub fn ws_address(url: &str) -> String {
    match url.split_once("://") {
        Some(("https", rest)) => format!("wss://{rest}"),
        Some(("http", rest)) => format!("ws://{rest}"),
        _ => url.to_owned(),
    }
}

/// The major of an index's model: a number, or the digits the broker wrote as text before
/// T-3963.
pub fn major_of(model: &Value) -> Option<u64> {
    let version = model.get("version")?;
    version
        .as_u64()
        .or_else(|| version.as_str().and_then(|text| text.parse().ok()))
}
