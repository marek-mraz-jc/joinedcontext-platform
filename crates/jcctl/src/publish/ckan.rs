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
use jc_core::kinds::{Audience, EndpointSpec, Representation};
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
}

impl Settings {
    /// Settings for one deployment.
    pub fn new(host: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            organization_title: None,
            language: None,
        }
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
    let url = settings.endpoint_url(spec.slug.as_str());
    let language = settings.language.as_deref();

    let mut payload = json!({
        "name": name,
        "owner_org": organization,
        "title": text_in(record.get("dct:title"), language).unwrap_or_else(|| name.to_owned()),
        "url": format!("{url}/"),
        "private": spec.audience != Audience::Public,
        "resources": resources(&spec.enabled_representations, &url, record),
        "extras": extras(record, &url),
    });
    if let Some(notes) = text_in(record.get("dct:description"), language) {
        payload["notes"] = json!(notes);
    }
    if let Some(license) = text(record.get("dct:license")) {
        // A licence IRI is not a CKAN licence id; the register holds ids like `cc-by`.
        if license.contains("://") {
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
    Ok(Some(payload))
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
            let mut resource = json!({
                "name": text(distribution.get("dct:title")).unwrap_or_else(|| url.to_owned()),
                "url": url,
                "format": format_of(url),
                "hash": sha256,
                "hash_algorithm": "sha256",
            });
            if let Some(media_type) = distribution.get("dcat:mediaType") {
                resource["mimetype"] = media_type.clone();
            }
            if let Some(bytes) = distribution.get("dcat:byteSize") {
                resource["size"] = bytes.clone();
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

fn extras(record: &Value, url: &str) -> Value {
    let mut extras: Vec<Value> = EXTRAS
        .iter()
        .filter_map(|(term, key)| {
            let value = flatten(record.get(*term)?)?;
            Some(json!({ "key": key, "value": value }))
        })
        .collect();
    // Who wrote this dataset and where it came from, so a manual edit in CKAN is visible
    // as drift rather than being silently overwritten without a trace (MF-08).
    extras.push(json!({ "key": "endpoint", "value": format!("{url}/") }));
    extras.push(json!({ "key": "generated_by", "value": GENERATOR }));
    Value::Array(extras)
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
        .into_iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .map(|keyword| json!({ "name": keyword }))
        .collect()
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
    calls: Vec<(String, Value)>,
    token: String,
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
            other => Err(Self::rejected(other, "unknown show action")),
        }
    }

    fn action(&mut self, action: &str, payload: &Value) -> Result<Value, CkanError> {
        self.calls.push((action.to_owned(), payload.clone()));
        if action.starts_with("datastore_") {
            return self.datastore(action, payload);
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
                        let (fields, _) = self
                            .tables
                            .get_mut(id)
                            .ok_or_else(|| Self::rejected(action, "unknown resource"))?;
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
            "datastore_delete" => {
                let id = Self::resource_of(action, payload)?;
                match payload.get("filters") {
                    None => {
                        self.tables.remove(&id);
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
