//! One Endpoint as one CKAN dataset (T-0316, EP-62…EP-67).
//!
//! Nothing about the dataset is authored a second time. The metadata is the DCAT-AP
//! record the Endpoint already answers with (EP-27, EP-63) and the resources are the
//! representations it already enables (EP-64), so the catalogue entry and the Endpoint
//! cannot drift apart: a policy narrowed today narrows the next publication too.
//!
//! This module builds payloads and decides which action to call. It opens no socket and
//! holds no credential: the CKAN API token is resolved from `spec.apiTokenRef` by the
//! reconciler and lives in the [`CkanApi`] implementation that carries the request, never
//! in a payload, a manifest or a log line (EP-67, CC-06).

use crate::loader::RawManifest;
use jc_core::kinds::ckan::{CkanInstanceSpec, CkanPublication};
use jc_core::kinds::{Audience, EndpointSpec, Licence, Representation};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

/// The tool the published dataset names as its origin (MF-08).
pub const GENERATOR: &str = "jcctl/ckan-publisher";

/// Where the endpoints answer, so a resource URL a citizen clicks resolves (EP-64).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// Public host of the Context Gateway, without a scheme.
    pub host: String,
    /// Title the CKAN organization is created with; the slug itself when absent.
    pub organization_title: Option<String>,
    /// The language the dataset's title and notes are taken in, from a record that carries
    /// several: the Endpoint space's `defaultLocale`. English, then the first, when absent or
    /// not in the record (EP-63).
    pub language: Option<String>,
    /// The installation's documentation base URL (branding `documentationBaseUrl`), under
    /// which the WebSocket resource links its guide (API/07). No link when absent.
    pub documentation: Option<String>,
}

impl Settings {
    /// Settings for one deployment.
    pub fn new(host: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            organization_title: None,
            language: None,
            documentation: None,
        }
    }

    /// Where the installation's documentation lives, for the usage texts (EP-108).
    pub fn documented_at(mut self, base: impl Into<String>) -> Self {
        self.documentation = Some(base.into().trim_end_matches('/').to_owned());
        self
    }

    /// The language the dataset is written in (EP-63).
    pub fn in_language(mut self, language: impl Into<String>) -> Self {
        self.language = Some(language.into());
        self
    }

    /// The organization title from the instance branding (Deployment/12).
    pub fn titled(mut self, title: impl Into<String>) -> Self {
        self.organization_title = Some(title.into());
        self
    }

    /// The public base URL of one Endpoint.
    pub fn endpoint_url(&self, slug: &str) -> String {
        format!("https://{}/api/endpoint/{slug}", self.host)
    }
}

/// What one publication run did to the dataset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The Endpoint declares no `spec.publish.ckan`, so there is nothing to publish.
    NotPublished,
    /// The dataset did not exist and was created.
    Created,
    /// The dataset existed and was brought back to the Endpoint's own description.
    Updated,
    /// The catalogue already matched the Endpoint; no call was made (CC-18).
    Unchanged,
    /// The dataset was removed (CC-19).
    Withdrawn,
}

/// Why the publication could not be built or carried out.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PublishError {
    /// The manifest is not an Endpoint.
    #[error("not an Endpoint manifest")]
    NotAnEndpoint,
    /// The Endpoint spec does not parse.
    #[error("endpoint spec: {0}")]
    Spec(String),
    /// Neither the publication nor the instance names a CKAN organization (EP-62).
    #[error("no CKAN organization: neither publish.ckan.organization nor organizationDefault")]
    NoOrganization,
    /// CKAN refused or could not be reached.
    #[error(transparent)]
    Api(#[from] CkanError),
}

/// Why CKAN did not answer.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CkanError {
    /// The instance could not be reached or refused the caller. Carries no token.
    #[error("CKAN unavailable: {0}")]
    Unavailable(String),
    /// The instance reached the action and rejected it.
    #[error("CKAN rejected {action}: {message}")]
    Rejected {
        /// The action that was refused.
        action: String,
        /// What CKAN said.
        message: String,
    },
}

/// The part of the CKAN Action API a publisher uses.
///
/// The implementation holds the API token resolved from the instance's `apiTokenRef` and
/// puts it in the `Authorization` header of the request it sends; no payload built here
/// ever carries it (EP-67).
pub trait CkanApi {
    /// A `*_show` action: the object, or `None` when CKAN answers `404`.
    fn show(&self, action: &str, name: &str) -> Result<Option<Value>, CkanError>;

    /// A writing action with its payload; the `result` CKAN answered with.
    fn action(&mut self, action: &str, payload: &Value) -> Result<Value, CkanError>;
}

/// The dataset one Endpoint becomes, or `None` when it declares no publication (EP-62).
pub fn package(
    endpoint: &RawManifest,
    instance: &CkanInstanceSpec,
    record: &Value,
    settings: &Settings,
) -> Result<Option<Value>, PublishError> {
    if endpoint.kind != "Endpoint" {
        return Err(PublishError::NotAnEndpoint);
    }
    let spec: EndpointSpec = serde_json::from_value(endpoint.spec.clone())
        .map_err(|e| PublishError::Spec(e.to_string()))?;
    let Some(publication) = spec.publish.as_ref().and_then(|p| p.ckan.as_ref()) else {
        return Ok(None);
    };
    let organization = organization(publication, instance)?;
    let name = publication.dataset_name(&endpoint.metadata.name);
    // The address a reader clicks is the one the Endpoint's own record names, not the host the
    // publisher read the record through: the Portal reads it on its own host, and the catalogue
    // showed that one beside a DCAT record naming the public one (T-4066).
    let url = public_base(record, spec.slug.as_str())
        .unwrap_or_else(|| settings.endpoint_url(spec.slug.as_str()));
    let language = settings.language.as_deref();
    let served = spec.served_representations();

    let mut payload = json!({
        "name": name,
        "owner_org": organization,
        "title": text_in(record.get("dct:title"), language).unwrap_or_else(|| name.to_owned()),
        "url": format!("{url}/"),
        "private": spec.audience != Audience::Public,
        "resources": resources(&served, &url, record),
        "extras": extras(record, &url, language),
    });
    if let Some(notes) = text_in(record.get("dct:description"), language) {
        payload["notes"] = json!(notes);
    }
    if let Some(license) = text(record.get("dct:license")) {
        // A licence IRI is not a CKAN licence id; the register holds ids like `cc-by`. One of
        // the EU licence table the Endpoint may name has a known id (EP-78).
        if license.contains("://") {
            if let Some(known) = Licence::ALL.iter().find(|l| l.iri() == license) {
                payload["license_id"] = json!(known.ckan_id());
            }
            push_extra(&mut payload, "license_url", &license);
        } else {
            payload["license_id"] = json!(license);
        }
    } else if let Some(license) = &publication.license {
        // The one term the record does not carry: the licence the organization chose, named
        // in the publication block (EP-62). A record that names one wins, so the dataset and
        // the Endpoint's own description never disagree (EP-63).
        payload["license_id"] = json!(license);
    }
    let tags = tags(record);
    if !tags.is_empty() {
        payload["tags"] = Value::Array(tags);
    }
    // The licence by its name where the register knows it, else as the record names it.
    let licence = payload["license_id"]
        .as_str()
        .map(|id| {
            Licence::ALL
                .iter()
                .find(|known| known.ckan_id() == id)
                .map_or(id, |known| known.title())
                .to_owned()
        })
        .or_else(|| text(record.get("dct:license")));
    describe(
        &mut payload["resources"],
        &served,
        &Usage {
            dataset: name,
            entity_type: record
                .get(EXAMPLE_TYPE)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|kind| !kind.is_empty()),
            url: &url,
            licence: licence.as_deref(),
            public: spec.audience == Audience::Public,
            documentation: settings.documentation.as_deref(),
        },
    );
    Ok(Some(payload))
}

/// The record's own `@id` when it is `https://{host}/api/endpoint/{slug}` for this slug and a
/// plain host; `None` keeps the publisher's host for a record that names anything else.
fn public_base(record: &Value, slug: &str) -> Option<String> {
    let id = record.get("@id")?.as_str()?.trim_end_matches('/');
    let host = id
        .strip_prefix("https://")?
        .strip_suffix(&format!("/api/endpoint/{slug}"))?;
    let plain = !host.is_empty()
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':'));
    plain.then(|| id.to_owned())
}

/// The member [`record`](crate::commands::publish_ckan::record) adds to the DCAT record: the
/// entity type the gateway's WebSocket guide subscribes its example to (T-4132), which the
/// usage texts name instead of a placeholder. It is no DCAT term and is never published.
pub const EXAMPLE_TYPE: &str = "jc:exampleType";

/// What a usage text says about the dataset it sits in (EP-108, API/02 §10).
struct Usage<'a> {
    dataset: &'a str,
    /// A type the Endpoint serves, `None` when the gateway's guide named none.
    entity_type: Option<&'a str>,
    url: &'a str,
    licence: Option<&'a str>,
    public: bool,
    documentation: Option<&'a str>,
}

/// Gives every surface resource the words a person or an AI client starts from (T-3152,
/// T-3886, EP-81, EP-108): a request to copy, the program to open it in, and who may read it.
/// The served surfaces are the first resources, in `served` order. Code sits in indented
/// blocks, which CKAN's Markdown shows as code. A text never carries a token: `$TOKEN` stands
/// where the caller's own goes (EP-110).
fn describe(resources: &mut Value, served: &[Representation], usage: &Usage) {
    let Some(resources) = resources.as_array_mut() else {
        return;
    };
    for (resource, representation) in resources.iter_mut().zip(served) {
        let address = resource["url"].as_str().unwrap_or_default().to_owned();
        resource["description"] = json!(usage.of(*representation, &address));
    }
}

impl Usage<'_> {
    fn of(&self, representation: Representation, address: &str) -> String {
        let access = if self.public {
            "Open to anyone, with no sign-in."
        } else {
            "Answers what the endpoint's policy lets the caller read: send a bearer token for the \
             endpoint; without one it answers 401."
        };
        let auth = if self.public {
            ""
        } else {
            " -H \"Authorization: Bearer $TOKEN\""
        };
        // What the catalogue keeps current, per resource kind (EP-109).
        let live = "Always live: the catalogue holds this link, never a copy.";
        let file = |what: &str| {
            let name = address.rsplit('/').next().unwrap_or("file");
            format!(
                "{what}, generated on each download from the endpoint's live data.\n\n    \
                 curl{auth} -o {name} '{address}'\n\n{access}"
            )
        };
        // A command a reader copies names a real type, never a placeholder (T-4132).
        let (first, kind) = match self.entity_type {
            Some(kind) => (format!("The first ten {kind} entities"), kind),
            None => ("The entity types it serves".to_owned(), ""),
        };
        let query = if kind.is_empty() {
            "types".to_owned()
        } else {
            format!("entities?type={kind}&limit=10")
        };
        match representation {
            Representation::Mcp => self.mcp(address),
            Representation::NgsiLd => format!(
                "The NGSI-LD API (ETSI GS CIM 009) of this dataset: query, filter and page its \
                 entities. {first}:\n\n    \
                 curl{auth} -H 'Accept: application/ld+json' '{address}{query}'\
                 \n\n{live} {access}"
            ),
            Representation::WebSocket => {
                let socket = address.replacen("https://", "wss://", 1);
                // Without a known type the subscription watches every entity's location.
                let entities = if kind.is_empty() {
                    "\"watchedAttributes\":[\"location\"]".to_owned()
                } else {
                    format!("\"entities\":[{{\"type\":{}}}]", json!(kind))
                };
                let browser = if self.public {
                    ""
                } else {
                    " A browser sends the token as the subprotocol `bearer.$TOKEN` beside \
                     `ngsi-ld-ws`, never in the URL."
                };
                let guide = self.documentation.map_or_else(String::new, |base| {
                    format!(
                        "\n\nThe message format, the limits and examples in JavaScript and \
                         Python: {base}/API/07-websocket"
                    )
                });
                format!(
                    "Live updates: open one WebSocket, subscribe to the entities you want, and \
                     each change arrives as a message, with no server of your own. With \
                     websocat, then type the subscription:\n\n    \
                     websocat --protocol ngsi-ld-ws{auth} '{socket}'\n    \
                     {{\"metadata\":{{\"x-op\":\"x-subscribe\",\"x-id\":\"s1\"}},\"body\":{{\"type\":\"Subscription\",{entities}}}}}\
                     \n\n{live} {access}{browser}{guide}"
                )
            }
            Representation::OgcFeatures => format!(
                "OGC API - Features, read-only: Part 1 (Core: collections and items), Part 2 \
                 (coordinate reference systems by reference) and Part 3 (Filtering with CQL2). \
                 {address}conformance lists the classes this endpoint serves. In QGIS: Layer, \
                 Add Layer, Add WFS / OGC API - Features Layer, a new connection with the URL \
                 {address}. With curl:\n\n    curl{auth} '{address}collections'\n\n{live} {access}"
            ),
            Representation::Odata => format!(
                "OData v4, read-only: each entity type is an entity set, with $filter, $select, \
                 $orderby and $top. In Excel: Data, Get Data, From Other Sources, From OData \
                 Feed, then the URL {address}. In Power BI: Get data, OData feed, the same URL. \
                 With curl:\n\n    curl{auth} '{address}'\n\n{live} {access}"
            ),
            Representation::Sta => format!(
                "OGC SensorThings API 1.1, read-only: Things, Datastreams and Observations.\
                 \n\n    curl{auth} '{address}Things'\n\n{live} {access}"
            ),
            Representation::Xlsx => file("An Excel spreadsheet that opens in Excel or LibreOffice"),
            Representation::Csv => file("A CSV table"),
            Representation::Parquet => file(
                "An Apache Parquet file with typed columns, for pandas, Polars, DuckDB or Spark",
            ),
            Representation::GeoJson => file("A GeoJSON feature collection that opens in QGIS"),
            Representation::Json => file("The entities as one JSON document"),
            Representation::Zip => file("Every file format of this dataset in one ZIP archive"),
        }
    }

    /// The words an AI client acts on (T-3152, EP-81): what the server is and which Endpoint
    /// it reads, its tools and what each returns, the licence, and a client configuration to
    /// copy. The tools are the gateway's own (`query_entities`, `get_entity`,
    /// `describe_schema`, `describe_access`).
    fn mcp(&self, server: &str) -> String {
        let who = if self.public {
            "answers anyone, with no sign-in, what the endpoint serves the public"
        } else {
            "answers what the endpoint lets the signed-in caller read: the client sends the caller's token"
        };
        let config = json!({ "mcpServers": { self.dataset: { "type": "http", "url": server } } });
        let licence = self.licence.map_or_else(
            || "Licence: as the dataset states.".to_owned(),
            |licence| format!("Licence: {licence}; the data is reused under it."),
        );
        format!(
            "For AI assistants: a Model Context Protocol server over this dataset's endpoint {}. \
             It {who}.\n\n\
             Tools: `query_entities` (the rows of an entity type, with an NGSI-LD `q` filter, `attrs`, \
             `count` and paging), `get_entity` (one entity by its URN), `describe_schema` (the data \
             model: read it first) and `describe_access` (what you may read).\n\n\
             {licence}\n\n\
             An MCP client reaches it with this configuration:\n\n    {config}\n",
            self.url
        )
    }
}

/// Creates or updates the dataset of one Endpoint, and the organization it lands in.
///
/// Idempotent: a second run over an unchanged Endpoint makes no writing call at all, so
/// `apply` converges rather than churning the catalogue (CC-18).
pub fn publish(
    api: &mut impl CkanApi,
    endpoint: &RawManifest,
    instance: &CkanInstanceSpec,
    record: &Value,
    settings: &Settings,
) -> Result<Outcome, PublishError> {
    let Some(desired) = package(endpoint, instance, record, settings)? else {
        return Ok(Outcome::NotPublished);
    };
    let organization = desired["owner_org"].as_str().unwrap_or_default().to_owned();
    if api.show("organization_show", &organization)?.is_none() {
        let title = settings
            .organization_title
            .clone()
            .unwrap_or_else(|| organization.clone());
        api.action(
            "organization_create",
            &json!({ "name": organization, "title": title }),
        )?;
    }

    let name = desired["name"].as_str().unwrap_or_default().to_owned();
    match api.show("package_show", &name)? {
        None => {
            api.action("package_create", &desired)?;
            Ok(Outcome::Created)
        }
        Some(live) if !deleted(&live) && same(&live, &desired) => Ok(Outcome::Unchanged),
        Some(live) => {
            let mut payload = desired;
            // CKAN addresses an update by id when it has one; the name is what a URL shows
            // and may be what this run is changing.
            if let Some(id) = live.get("id") {
                payload["id"] = id.clone();
            }
            // A withdrawn dataset keeps its name in CKAN, so publishing it again is an
            // update that brings it back rather than a creation CKAN would refuse.
            if deleted(&live) {
                payload["state"] = json!("active");
            }
            // `package_update` replaces the resource list, so the DataStore table the
            // mirror created (EP-65) is carried over by id or CKAN drops it with its rows.
            let kept: Vec<Value> = live
                .get("resources")
                .and_then(Value::as_array)
                .map(|resources| {
                    resources
                        .iter()
                        .filter(|resource| is_datastore(resource))
                        .cloned()
                        .collect()
                })
                .unwrap_or_default();
            if let Some(resources) = payload["resources"].as_array_mut() {
                resources.extend(kept);
            }
            api.action("package_update", &payload)?;
            Ok(Outcome::Updated)
        }
    }
}

/// Removes the dataset an Endpoint no longer publishes (EP-62, CC-19).
///
/// Removing what is not there succeeds: a publication run has to be re-runnable after a
/// partial failure. A sysadmin token still sees a deleted dataset in `package_show`, so a
/// dataset CKAN already marked deleted is "not there" too.
pub fn withdraw(api: &mut impl CkanApi, name: &str) -> Result<Outcome, PublishError> {
    match api.show("package_show", name)? {
        None => return Ok(Outcome::Unchanged),
        Some(live) if deleted(&live) => return Ok(Outcome::Unchanged),
        Some(_) => {}
    }
    api.action("package_delete", &json!({ "id": name }))?;
    Ok(Outcome::Withdrawn)
}

/// Whether the record lists a model's artifacts and not its LinkML: an Endpoint with a model
/// lists `model.linkml.yaml` in its dataset, and one whose record lacks it still links the
/// schema index, which the publisher reports (EP-68, T-3886).
pub fn missing_linkml(record: &Value) -> bool {
    let artifacts = schema_resources(record);
    !artifacts.is_empty()
        && !artifacts
            .iter()
            .any(|artifact| artifact["format"] == "LinkML")
}

/// The W3C Data Quality Vocabulary, which DCAT-AP's own quality terms build on (DM-95).
pub const DQV: &str = "http://www.w3.org/ns/dqv#";
/// The metric of the share of checks a dataset's writes passed over seven days.
pub const PASS_RATE: &str = "https://joinedcontext.com/ns/quality#passRate";
/// The metric of the age, in seconds, of the newest data when the space was last read.
pub const FRESHNESS: &str = "https://joinedcontext.com/ns/quality#freshness";

/// What the platform measured of one dataset's data (DM-95, ADR-N-046 2.7). `None` is "not
/// measured", which adds nothing, never a zero.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Measurements {
    /// Checks passed over all checks of the space's quality rules in seven days, 0..=1.
    pub pass_rate: Option<f64>,
    /// Seconds between the newest data and the daily quality run that read it.
    pub freshness_seconds: Option<i64>,
}

/// `record` with a `dqv:hasQualityMeasurement` for each measurement taken (DM-95).
pub fn measured(record: &Value, measurements: &Measurements) -> Value {
    let node = |metric: &str, value: Value| {
        json!({
            "@type": "dqv:QualityMeasurement",
            "dqv:isMeasurementOf": { "@id": metric },
            "dqv:value": value,
        })
    };
    let nodes: Vec<Value> = [
        measurements
            .pass_rate
            .map(|rate| node(PASS_RATE, json!(rate))),
        measurements
            .freshness_seconds
            .map(|seconds| node(FRESHNESS, json!(seconds))),
    ]
    .into_iter()
    .flatten()
    .collect();
    let mut record = record.clone();
    if nodes.is_empty() {
        return record;
    }
    if let Some(context) = record.get_mut("@context").and_then(Value::as_object_mut) {
        context.entry("dqv").or_insert_with(|| json!(DQV));
    }
    record["dqv:hasQualityMeasurement"] = Value::Array(nodes);
    record
}

/// The record's quality measurements as the extras a CKAN dataset page shows: ckanext-dcat maps
/// only `dqv:hasQualityAnnotation`, so they are plain keys (DM-95).
fn quality_extras(record: &Value) -> Vec<Value> {
    let measurements = record
        .get("dqv:hasQualityMeasurement")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    measurements
        .iter()
        .filter_map(|node| {
            let key = match node.pointer("/dqv:isMeasurementOf/@id")?.as_str()? {
                PASS_RATE => "quality_pass_rate",
                FRESHNESS => "quality_freshness_seconds",
                _ => return None,
            };
            let value = node.get("dqv:value")?;
            Some(json!({ "key": key, "value": value.to_string() }))
        })
        .collect()
}

/// Whether CKAN holds the dataset as deleted: `package_delete` marks rather than purges.
fn deleted(live: &Value) -> bool {
    live.get("state").and_then(Value::as_str) == Some("deleted")
}

/// Whether a live resource is a DataStore table rather than one of the representation
/// resources this module writes; CKAN marks the ones `datastore_create` made (EP-65).
fn is_datastore(resource: &Value) -> bool {
    resource.get("url_type").and_then(Value::as_str) == Some("datastore")
}

/// One resource per enabled representation, the schema index, and every schema
/// artifact the record lists (EP-64, EP-68).
///
/// The URL is always the representation's own URL under the Endpoint, so a download
/// passes the gateway and its policy set rather than a copy nobody governs (EP-66).
fn resources(enabled: &[Representation], url: &str, record: &Value) -> Value {
    let mut resources: Vec<Value> = enabled
        .iter()
        .map(|representation| {
            let (path, format, mimetype, title) = distribution(*representation);
            json!({
                "name": title,
                "url": format!("{url}{path}"),
                "format": format,
                "mimetype": mimetype,
            })
        })
        .collect();
    // The index lists every artifact of every model version, so a consumer can validate
    // what it downloaded without the publisher knowing which versions exist (EP-46).
    resources.push(json!({
        "name": "Schema artifacts",
        "url": format!("{url}/schema/index.json"),
        "format": "JSON",
        "mimetype": "application/json",
        "description": "The data model in every formalism, for every model version, each artifact \
                        with its sha256 (EP-68).",
    }));
    resources.extend(schema_resources(record));
    Value::Array(resources)
}

/// One resource per schema artifact the record lists, so a citizen browsing the catalogue
/// finds the model in every formalism next to the data itself (EP-68).
///
/// Read off the DCAT-AP record rather than rebuilt: a distribution with an `spdx:checksum`
/// is a schema artifact, and the digest travels into CKAN's own `hash` field so a download
/// can be checked without asking the Endpoint again.
fn schema_resources(record: &Value) -> Vec<Value> {
    let Some(distributions) = record.get("dcat:distribution").and_then(Value::as_array) else {
        return Vec::new();
    };
    distributions
        .iter()
        .filter_map(|distribution| {
            let sha256 = distribution
                .get("spdx:checksum")?
                .get("spdx:checksumValue")?
                .as_str()?;
            let url = distribution.get("dcat:accessURL")?.as_str()?;
            let format = format_of(url);
            // A published Mapping's file sits under `schema/v{n}/mappings/` (EP-53).
            let description = if url.contains("/mappings/") {
                format!(
                    "A mapping of the data model to another model, as {format}; sha256 {sha256}."
                )
            } else {
                format!("The data model as {format}; sha256 {sha256}.")
            };
            let mut resource = json!({
                "name": text(distribution.get("dct:title")).unwrap_or_else(|| url.to_owned()),
                "url": url,
                "description": description,
                "format": format,
                "hash": sha256,
                "hash_algorithm": "sha256",
            });
            if let Some(media_type) = distribution.get("dcat:mediaType") {
                resource["mimetype"] = mimetype(media_type);
            }
            // A typed literal in the record, a number in CKAN.
            let bytes = distribution.get("dcat:byteSize");
            if let Some(size) = bytes
                .and_then(Value::as_u64)
                .or_else(|| bytes.and_then(Value::as_str)?.parse().ok())
            {
                resource["size"] = json!(size);
            }
            Some(resource)
        })
        .collect()
}

/// The CKAN format label of one schema artifact, by the file name the Endpoint serves it
/// under (EP-46). An unknown artifact keeps its extension rather than being dropped.
fn format_of(url: &str) -> String {
    let file_name = url.rsplit('/').next().unwrap_or(url);
    match file_name {
        "model.linkml.yaml" => "LinkML",
        "model.schema.json" => "JSON Schema",
        "context.jsonld" => "JSON-LD",
        "model.shacl.ttl" => "SHACL",
        "model.owl.ttl" => "OWL",
        "model.rdf.ttl" => "RDF",
        "model.md" => "Markdown",
        name if name.ends_with(".linkml-map.yaml") => "LinkML-Map",
        name if name.ends_with(".blobl") => "Bloblang",
        name if name.ends_with(".ir.json") => "Mapping IR",
        _ => return file_name.rsplit('.').next().unwrap_or("").to_uppercase(),
    }
    .to_owned()
}

/// Path under the Endpoint, CKAN format, media type and resource title (API/02).
const fn distribution(
    representation: Representation,
) -> (&'static str, &'static str, &'static str, &'static str) {
    match representation {
        Representation::NgsiLd => (
            "/ngsi-ld/v1/",
            "NGSI-LD",
            "application/ld+json",
            "NGSI-LD API",
        ),
        Representation::Mcp => ("/mcp", "MCP", "application/json", "Model Context Protocol"),
        Representation::GeoJson => (
            "/file.geojson",
            "GeoJSON",
            "application/geo+json",
            "GeoJSON",
        ),
        Representation::Csv => ("/file.csv", "CSV", "text/csv", "CSV"),
        Representation::Xlsx => (
            "/file.xlsx",
            "XLSX",
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            "Excel spreadsheet",
        ),
        Representation::Parquet => (
            "/file.parquet",
            "PARQUET",
            "application/vnd.apache.parquet",
            "Apache Parquet",
        ),
        Representation::Json => ("/file.json", "JSON", "application/json", "JSON"),
        Representation::Zip => ("/file.zip", "ZIP", "application/zip", "Export bundle"),
        Representation::OgcFeatures => (
            "/ogc/features/",
            "OGCFEAT",
            "application/geo+json",
            "OGC API - Features",
        ),
        Representation::Sta => (
            "/sta/v1.1/",
            "STA",
            "application/json",
            "OGC SensorThings API",
        ),
        Representation::Odata => ("/odata/v4/", "OData", "application/json", "OData v4"),
        Representation::WebSocket => (
            "/ws",
            "WebSocket",
            "application/json",
            "Live updates over WebSocket",
        ),
    }
}

/// The DCAT-AP terms that become CKAN extras, in the order a reader sees them.
const EXTRAS: &[(&str, &str)] = &[
    ("dct:identifier", "identifier"),
    ("dct:language", "language"),
    ("dct:accrualPeriodicity", "frequency"),
    ("dct:publisher", "publisher"),
    ("dcat:contactPoint", "contact_point"),
    ("dct:spatial", "spatial"),
    ("dct:temporal", "temporal"),
    ("dct:conformsTo", "conforms_to"),
    ("adms:status", "status"),
];

fn extras(record: &Value, url: &str, language: Option<&str>) -> Value {
    let mut extras: Vec<Value> = EXTRAS
        .iter()
        .filter_map(|(term, key)| {
            let value = record.get(*term)?;
            // A typed node (a `foaf:Agent`, a `vcard:Kind`, a period) is written field by
            // field below, under the keys ckanext-dcat reads, not as a JSON dump.
            if value.get("@type").is_some() {
                return None;
            }
            // `spatial` is GeoJSON to the spatial extension; a location IRI goes to its own key.
            let key = match (value, *key) {
                (Value::String(_) | Value::Array(_), "spatial") => "spatial_uri",
                (_, key) => key,
            };
            Some(json!({ "key": key, "value": flatten(value)? }))
        })
        .collect();
    extras.extend(catalog_extras(record, language));
    extras.extend(quality_extras(record));
    // Who wrote this dataset and where it came from, so a manual edit in CKAN is visible
    // as drift rather than being silently overwritten without a trace (MF-08).
    extras.push(json!({ "key": "endpoint", "value": format!("{url}/") }));
    extras.push(json!({ "key": "generated_by", "value": GENERATOR }));
    Value::Array(extras)
}

/// The structured terms of the catalogue block, under the keys ckanext-dcat reads (EP-78).
fn catalog_extras(record: &Value, language: Option<&str>) -> Vec<Value> {
    let mut out = Vec::new();
    let mut put = |key: &str, value: Option<String>| {
        if let Some(value) = value.filter(|v| !v.is_empty()) {
            out.push(json!({ "key": key, "value": value }));
        }
    };
    let publisher = record.get("dct:publisher");
    put(
        "publisher_name",
        text_in(publisher.and_then(|p| p.get("foaf:name")), language),
    );
    put("publisher_uri", text(publisher.and_then(|p| p.get("@id"))));
    let contact = record.get("dcat:contactPoint");
    put(
        "contact_name",
        text(contact.and_then(|c| c.get("vcard:fn"))),
    );
    put(
        "contact_email",
        text(contact.and_then(|c| c.get("vcard:hasEmail")))
            .map(|email| email.trim_start_matches("mailto:").to_owned()),
    );
    let temporal = record.get("dct:temporal");
    put(
        "temporal_start",
        text(temporal.and_then(|t| t.get("dcat:startDate"))),
    );
    put(
        "temporal_end",
        text(temporal.and_then(|t| t.get("dcat:endDate"))),
    );
    if let Some(Value::Array(themes)) = record.get("dcat:theme") {
        put("theme", serde_json::to_string(themes).ok());
    }
    // The credit the licence asks for, which the record carries on every distribution as
    // `dct:rights`; the original datasets, a list ckanext-dcat reads back as `dct:source`; and the
    // Pipeline that fills the space (EP-78, T-4417).
    let rights = record
        .get("dcat:distribution")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find_map(|d| d.pointer("/dct:rights/rdfs:label"));
    put("rights", text_in(rights, language));
    if let Some(Value::Array(sources)) = record.get("dct:source") {
        put("source", serde_json::to_string(sources).ok());
    }
    put(
        "was_generated_by",
        text(record.pointer("/prov:wasGeneratedBy/rdfs:label")),
    );
    out
}

/// A `dcat:mediaType` as CKAN's `mimetype`: the record names the IANA IRI (EP-78).
fn mimetype(media_type: &Value) -> Value {
    match media_type.as_str() {
        Some(iri) => json!(iri
            .strip_prefix("https://www.iana.org/assignments/media-types/")
            .unwrap_or(iri)),
        None => media_type.clone(),
    }
}

fn push_extra(payload: &mut Value, key: &str, value: &str) {
    if let Some(extras) = payload["extras"].as_array_mut() {
        extras.push(json!({ "key": key, "value": value }));
    }
}

/// `dcat:keyword`, as CKAN tags.
fn tags(record: &Value) -> Vec<Value> {
    let keywords = match record.get("dcat:keyword") {
        Some(Value::Array(items)) => items.iter().filter_map(text_of).collect::<Vec<_>>(),
        Some(other) => text_of(other).into_iter().collect(),
        None => Vec::new(),
    };
    // A language map repeats one concept per locale; CKAN tags are a set.
    keywords
        .iter()
        .filter_map(|keyword| tag_name(keyword))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .map(|keyword| json!({ "name": keyword }))
        .collect()
}

/// The shortest and the longest tag CKAN holds (`ckan.model.tag`).
const TAG_LENGTH: std::ops::RangeInclusive<usize> = 2..=100;

/// `keyword` as a CKAN tag, `None` when nothing of it is left (T-2982).
///
/// CKAN refuses the whole `package_update` over one tag outside letters, digits, space, `-`,
/// `_` and `.`, so every other character becomes a space (`P+R` is `P R`), spaces collapse and
/// the tag is cut to CKAN's length. The DCAT-AP record keeps the keyword as it was written.
fn tag_name(keyword: &str) -> Option<String> {
    let allowed = |c: char| c.is_alphanumeric() || matches!(c, ' ' | '-' | '_' | '.');
    let spaced: String = keyword
        .chars()
        .map(|c| if allowed(c) { c } else { ' ' })
        .collect();
    let tag: String = spaced
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(*TAG_LENGTH.end())
        .collect();
    let tag = tag.trim_end().to_owned();
    TAG_LENGTH.contains(&tag.chars().count()).then_some(tag)
}

/// The organization the dataset lands in: the publication's, else the instance default.
fn organization(
    publication: &CkanPublication,
    instance: &CkanInstanceSpec,
) -> Result<String, PublishError> {
    publication
        .organization
        .clone()
        .or_else(|| instance.organization_default.clone())
        .ok_or(PublishError::NoOrganization)
}

/// One string out of a JSON-LD value: a plain string, a language map, or `@value`.
fn text(value: Option<&Value>) -> Option<String> {
    value.and_then(text_of)
}

/// [`text`] in `language` when the record carries it, which is how a Slovak body's dataset
/// keeps its Slovak title rather than the English one beside it (EP-63).
fn text_in(value: Option<&Value>, language: Option<&str>) -> Option<String> {
    let value = value?;
    if let (Some(language), Value::Array(items)) = (language, value) {
        let chosen = items.iter().find_map(|item| {
            (item.get("@language")?.as_str()? == language)
                .then(|| item.get("@value")?.as_str())
                .flatten()
        });
        if let Some(text) = chosen {
            return Some(text.to_owned());
        }
    }
    text_of(value)
}

fn text_of(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Array(items) => {
            let by_language: BTreeMap<&str, &str> = items
                .iter()
                .filter_map(|item| {
                    Some((
                        item.get("@language")?.as_str()?,
                        item.get("@value")?.as_str()?,
                    ))
                })
                .collect();
            by_language
                .get("en")
                .or_else(|| by_language.values().next())
                .map(|text| (*text).to_owned())
                .or_else(|| items.first().and_then(text_of))
        }
        Value::Object(_) => value
            .get("@value")
            .and_then(Value::as_str)
            .map(str::to_owned),
        _ => None,
    }
}

/// A CKAN extra is a string; a term that is a structure keeps its JSON, which is what the
/// spatial extension expects of `dct:spatial` anyway.
fn flatten(value: &Value) -> Option<String> {
    text_of(value).or_else(|| match value {
        Value::Object(_) | Value::Array(_) => serde_json::to_string(value).ok(),
        Value::Null => None,
        other => Some(other.to_string()),
    })
}

/// Whether the live dataset already says what this run would say.
///
/// Only the fields the publisher owns are compared, in the shape `package_show` answers
/// them: CKAN adds ids, timestamps and revision bookkeeping of its own, answers the
/// organization by id beside a nested `organization`, decorates every tag and sorts the
/// extras, and none of that is drift (CC-18).
fn same(live: &Value, desired: &Value) -> bool {
    desired.as_object().is_some_and(|fields| {
        fields.iter().all(|(key, value)| match key.as_str() {
            "resources" => {
                managed_resources(live.get("resources")) == managed_resources(Some(value))
            }
            "owner_org" => {
                live.get("owner_org") == Some(value)
                    || live.get("organization").and_then(|org| org.get("name")) == Some(value)
            }
            "tags" => tag_names(live.get("tags")) == tag_names(Some(value)),
            "extras" => extras_by_key(live.get("extras")) == extras_by_key(Some(value)),
            _ => live.get(key) == Some(value),
        })
    })
}

fn tag_names(tags: Option<&Value>) -> BTreeSet<String> {
    tags.and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|tag| tag.get("name")?.as_str())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn extras_by_key(extras: Option<&Value>) -> BTreeMap<String, Value> {
    extras
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|extra| {
                    Some((
                        extra.get("key")?.as_str()?.to_owned(),
                        extra.get("value").cloned().unwrap_or(Value::Null),
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn managed_resources(resources: Option<&Value>) -> Vec<Value> {
    resources
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter(|resource| !is_datastore(resource))
                .map(|resource| {
                    json!({
                        "name": resource.get("name"),
                        "url": resource.get("url"),
                        "format": resource.get("format"),
                        "mimetype": resource.get("mimetype"),
                        "description": resource.get("description"),
                        // A regenerated model keeps its file names and changes its digest;
                        // without this a stale hash would sit in CKAN as "unchanged". CKAN
                        // answers `""` for a resource that has none, which is no hash either.
                        "hash": resource.get("hash").filter(|hash| hash.as_str() != Some("")),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// A CKAN instance held in memory: the test double, and the record of what a run called.
///
/// It carries a token the way a real transport does, so a test can assert that no payload
/// this module builds ever contains it (EP-67).
#[derive(Debug, Default, Clone)]
pub struct InMemoryCkan {
    organizations: BTreeSet<String>,
    packages: BTreeMap<String, Value>,
    /// DataStore tables by resource id: the declared fields, and the rows by primary key.
    tables: BTreeMap<String, (Vec<Value>, BTreeMap<String, Value>)>,
    /// Resource views by resource id, in creation order.
    views: BTreeMap<String, Vec<Value>>,
    calls: Vec<(String, Value)>,
    token: String,
    /// Writes so far: the clock `metadata_modified` is read off.
    revision: u64,
}

impl InMemoryCkan {
    /// An empty instance, as a fresh CKAN looks.
    pub fn new() -> Self {
        Self::default()
    }

    /// An organization that already exists.
    pub fn with_organization(mut self, name: &str) -> Self {
        self.organizations.insert(name.to_owned());
        self
    }

    /// The API token the transport would send, which no payload may contain.
    pub fn with_token(mut self, token: &str) -> Self {
        self.token = token.to_owned();
        self
    }

    /// The dataset under `name`, as CKAN holds it.
    pub fn package(&self, name: &str) -> Option<&Value> {
        self.packages.get(name)
    }

    /// The actions this instance was called with, in order.
    pub fn actions(&self) -> Vec<&str> {
        self.calls
            .iter()
            .map(|(action, _)| action.as_str())
            .collect()
    }

    /// Every call with its payload, for assertions on what was sent.
    pub fn calls(&self) -> impl Iterator<Item = (&str, &Value)> {
        self.calls
            .iter()
            .map(|(action, payload)| (action.as_str(), payload))
    }

    /// The token this instance holds, so a test can look for it where it must not be.
    pub fn token(&self) -> &str {
        &self.token
    }

    /// The DataStore rows under one resource, keyed by their primary key (EP-65).
    pub fn rows(&self, resource_id: &str) -> Option<&BTreeMap<String, Value>> {
        self.tables.get(resource_id).map(|(_, rows)| rows)
    }

    /// The views of one resource, in creation order; empty for a resource without any.
    pub fn views(&self, resource_id: &str) -> &[Value] {
        self.views.get(resource_id).map_or(&[], Vec::as_slice)
    }

    /// The DataStore fields declared for one resource, in declaration order.
    pub fn table_fields(&self, resource_id: &str) -> Option<&[Value]> {
        self.tables
            .get(resource_id)
            .map(|(fields, _)| fields.as_slice())
    }

    fn rejected(action: &str, message: &str) -> CkanError {
        CkanError::Rejected {
            action: action.to_owned(),
            message: message.to_owned(),
        }
    }
}

impl CkanApi for InMemoryCkan {
    fn show(&self, action: &str, name: &str) -> Result<Option<Value>, CkanError> {
        match action {
            "package_show" => Ok(self.packages.get(name).cloned()),
            "organization_show" => Ok(self
                .organizations
                .contains(name)
                .then(|| json!({ "name": name }))),
            "resource_show" => Ok(self
                .tables
                .get(name)
                .map(|(fields, _)| json!({ "resource_id": name, "id": name, "fields": fields }))),
            // CKAN answers 404 for a resource it does not hold, and a list for one it does.
            "resource_view_list" => Ok(self
                .tables
                .contains_key(name)
                .then(|| json!(self.views(name)))),
            other => Err(Self::rejected(other, "unknown show action")),
        }
    }

    fn action(&mut self, action: &str, payload: &Value) -> Result<Value, CkanError> {
        self.calls.push((action.to_owned(), payload.clone()));
        if action.starts_with("datastore_") {
            return self.datastore(action, payload);
        }
        if action == "resource_view_create" {
            let id = Self::resource_of(action, payload)?;
            if !self.tables.contains_key(&id) {
                return Err(Self::rejected(action, "unknown resource"));
            }
            let views = self.views.entry(id).or_default();
            let mut view = payload.clone();
            view["id"] = json!(format!("view-{}", views.len() + 1));
            views.push(view.clone());
            return Ok(view);
        }
        let name = payload
            .get("name")
            .or_else(|| payload.get("id"))
            .and_then(Value::as_str)
            .ok_or_else(|| Self::rejected(action, "the payload names no resource"))?
            .to_owned();
        match action {
            "organization_create" => {
                self.organizations.insert(name);
                Ok(json!({}))
            }
            "package_create" | "package_update" => {
                let organization = payload
                    .get("owner_org")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if !self.organizations.contains(organization) {
                    return Err(Self::rejected(action, "unknown organization"));
                }
                let key = self.key_of(payload, &name);
                if action == "package_create" && self.packages.contains_key(&key) {
                    return Err(Self::rejected(action, "that dataset already exists"));
                }
                let mut stored = payload.clone();
                // CKAN mints an id on creation and keeps it for the life of the dataset.
                stored["id"] = json!(format!("pkg-{key}"));
                stored["metadata_modified"] = json!(self.tick());
                // and answers an empty `hash` for every resource that was given none.
                if let Some(resources) = stored["resources"].as_array_mut() {
                    for resource in resources {
                        if resource.get("hash").is_none() {
                            resource["hash"] = json!("");
                        }
                    }
                }
                self.packages.insert(key, stored);
                Ok(json!({}))
            }
            "package_delete" => {
                self.packages.remove(&self.key_of(payload, &name));
                Ok(json!({}))
            }
            "resource_patch" => {
                let stamp = self.tick();
                let listed = self.packages.values_mut().find_map(|package| {
                    let index = package["resources"]
                        .as_array()?
                        .iter()
                        .position(|r| r["id"] == json!(name))?;
                    Some((package, index))
                });
                let Some((package, index)) = listed else {
                    // A table made without its dataset, as the DataStore tests make them.
                    return if self.tables.contains_key(&name) {
                        Ok(payload.clone())
                    } else {
                        Err(Self::rejected(action, "unknown resource"))
                    };
                };
                let resource = &mut package["resources"][index];
                for (key, value) in payload.as_object().into_iter().flatten() {
                    resource[key] = value.clone();
                }
                let patched = resource.clone();
                // A resource that changes changes its dataset, as CKAN's resource_update does.
                package["metadata_modified"] = json!(stamp);
                Ok(patched)
            }
            "resource_delete" => {
                let mut found = false;
                for package in self.packages.values_mut() {
                    if let Some(resources) = package["resources"].as_array_mut() {
                        let before = resources.len();
                        resources.retain(|resource| resource["id"] != json!(name));
                        found |= resources.len() != before;
                    }
                }
                if !found {
                    return Err(Self::rejected(action, "unknown resource"));
                }
                self.tables.remove(&name);
                self.views.remove(&name);
                Ok(json!({}))
            }
            other => Err(Self::rejected(other, "unknown action")),
        }
    }
}

impl InMemoryCkan {
    /// The DataStore half of the Action API: create or extend a table, upsert rows, delete.
    ///
    /// It enforces the one rule that matters to a caller building payloads: an upsert
    /// naming a field the table does not declare is refused, the way CKAN refuses it.
    fn datastore(&mut self, action: &str, payload: &Value) -> Result<Value, CkanError> {
        match action {
            "datastore_create" => {
                let declared = payload
                    .get("fields")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                match payload.get("resource_id").and_then(Value::as_str) {
                    Some(id) => {
                        // A resource whose table was dropped gets a new one, as CKAN does: the
                        // resource outlives `datastore_delete` without filters.
                        let (fields, _) = self
                            .tables
                            .entry(id.to_owned())
                            .or_insert_with(|| (Vec::new(), BTreeMap::new()));
                        fields.extend(declared);
                        Ok(json!({ "resource_id": id }))
                    }
                    None => {
                        let id = payload["resource"]["name"]
                            .as_str()
                            .ok_or_else(|| Self::rejected(action, "the resource has no name"))?
                            .to_owned();
                        if self.tables.contains_key(&id) {
                            return Err(Self::rejected(action, "that table already exists"));
                        }
                        // CKAN lists the resource the table was created under on its dataset.
                        let package = payload["resource"]["package_id"]
                            .as_str()
                            .unwrap_or_default();
                        let key = self.key_of(&json!({ "id": package }), package);
                        if let Some(resources) = self
                            .packages
                            .get_mut(&key)
                            .and_then(|live| live["resources"].as_array_mut())
                        {
                            resources.push(
                                json!({ "id": id, "name": id, "url_type": "datastore", "hash": "" }),
                            );
                        }
                        self.tables.insert(id.clone(), (declared, BTreeMap::new()));
                        Ok(json!({ "resource_id": id }))
                    }
                }
            }
            "datastore_upsert" => {
                let id = Self::resource_of(action, payload)?;
                let (fields, rows) = self
                    .tables
                    .get_mut(&id)
                    .ok_or_else(|| Self::rejected(action, "unknown resource"))?;
                let known: BTreeSet<&str> =
                    fields.iter().filter_map(|f| f["id"].as_str()).collect();
                for record in payload["records"].as_array().into_iter().flatten() {
                    let members = record
                        .as_object()
                        .ok_or_else(|| Self::rejected(action, "a record is not an object"))?;
                    if let Some(unknown) = members.keys().find(|key| !known.contains(key.as_str()))
                    {
                        return Err(Self::rejected(
                            action,
                            &format!("the table has no field '{unknown}'"),
                        ));
                    }
                    let key = members
                        .get("entity_id")
                        .and_then(Value::as_str)
                        .ok_or_else(|| Self::rejected(action, "a record has no entity_id"))?
                        .to_owned();
                    rows.insert(key, record.clone());
                }
                Ok(json!({}))
            }
            "datastore_search" => {
                let id = Self::resource_of(action, payload)?;
                let (_, rows) = self
                    .tables
                    .get(&id)
                    .ok_or_else(|| Self::rejected(action, "unknown resource"))?;
                let count = |key: &str| {
                    payload
                        .get(key)
                        .and_then(Value::as_u64)
                        .and_then(|n| usize::try_from(n).ok())
                };
                let fields: Option<Vec<&str>> = payload
                    .get("fields")
                    .and_then(Value::as_array)
                    .map(|fields| fields.iter().filter_map(Value::as_str).collect());
                let records: Vec<Value> = rows
                    .values()
                    .skip(count("offset").unwrap_or(0))
                    .take(count("limit").unwrap_or(100))
                    .map(|row| match &fields {
                        Some(fields) => Value::Object(
                            fields
                                .iter()
                                .filter_map(|field| {
                                    Some(((*field).to_owned(), row.get(*field)?.clone()))
                                })
                                .collect(),
                        ),
                        None => row.clone(),
                    })
                    .collect();
                Ok(json!({ "records": records, "total": rows.len() }))
            }
            "datastore_delete" => {
                let id = Self::resource_of(action, payload)?;
                match payload.get("filters") {
                    None => {
                        self.tables.remove(&id);
                        self.views.remove(&id);
                    }
                    Some(filters) => {
                        let (_, rows) = self
                            .tables
                            .get_mut(&id)
                            .ok_or_else(|| Self::rejected(action, "unknown resource"))?;
                        for key in filters["entity_id"].as_array().into_iter().flatten() {
                            if let Some(key) = key.as_str() {
                                rows.remove(key);
                            }
                        }
                    }
                }
                Ok(json!({}))
            }
            other => Err(Self::rejected(other, "unknown action")),
        }
    }

    /// The next `metadata_modified`, later than every one before it.
    fn tick(&mut self) -> String {
        self.revision += 1;
        format!("2026-01-01T00:00:00.{:06}", self.revision)
    }

    fn resource_of(action: &str, payload: &Value) -> Result<String, CkanError> {
        payload
            .get("resource_id")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| Self::rejected(action, "the payload names no resource"))
    }
}

impl InMemoryCkan {
    /// A dataset is addressed by id or by name; the store is keyed by name either way.
    fn key_of(&self, payload: &Value, name: &str) -> String {
        let by_id = payload.get("id").and_then(Value::as_str).and_then(|id| {
            self.packages
                .iter()
                .find(|(_, package)| package.get("id").and_then(Value::as_str) == Some(id))
                .map(|(key, _)| key.clone())
        });
        by_id.unwrap_or_else(|| name.to_owned())
    }
}
