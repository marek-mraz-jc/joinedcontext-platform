//! A `catalogue` knowledge source (T-3225, AG-116): the project's own NGSI-LD catalogue, one
//! page per Endpoint of its context spaces.
//!
//! A page is written from the repository the worker already reads, so it needs no credential:
//! the Endpoint's title, space, audience and address, its CKAN dataset when it publishes one, and
//! per entity type of the space's model the class description and every attribute the Endpoint
//! serves, with its description and unit. A public Endpoint's page also carries each type's
//! entity count, read anonymously through the gateway as any visitor could read it. A `public`
//! source holds public Endpoints alone, so nothing of a private space reaches a public channel.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Component, Path, PathBuf};

use jc_core::kinds::data_model::{DataModelLifecycle, DataModelSpec};
use jc_core::kinds::EndpointSpec;
use jcctl::loader::Repository;
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use url::Url;

use crate::crawl::Sink;
use crate::Error;

/// One attribute of an entity type, as the space's model describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attribute {
    pub name: String,
    /// `Property`, `Relationship`, `GeoProperty` or `LanguageProperty`.
    pub kind: String,
    pub description: Option<String>,
    /// The UCUM code of a quantity, when the model names one.
    pub unit: Option<String>,
}

/// One entity type of a space's model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntityType {
    pub class: String,
    pub description: Option<String>,
    pub attributes: Vec<Attribute>,
}

/// What one Endpoint's page is written from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointPage {
    pub name: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub space: String,
    pub slug: String,
    /// `public`, `organization` or `project-list`.
    pub audience: String,
    /// The CKAN dataset page, when the Endpoint publishes to a catalogue of the project.
    pub ckan_dataset: Option<String>,
    pub types: Vec<EntityType>,
}

impl EndpointPage {
    pub fn is_public(&self) -> bool {
        self.audience == "public"
    }

    /// What a passage of this page cites: the CKAN dataset when there is one, else the
    /// Endpoint's own public address under `endpoint_base`.
    pub fn citation(&self, endpoint_base: &Url) -> Result<String, Error> {
        if let Some(dataset) = &self.ckan_dataset {
            return Ok(dataset.clone());
        }
        endpoint_base
            .join(&format!("api/endpoint/{}/schema/index.json", self.slug))
            .map(String::from)
            .map_err(|err| Error::Crawl(format!("the Endpoint address: {err}")))
    }
}

/// The entity types a model's generated JSON Schema declares, each attribute in the order the
/// schema lists it; `id` and `type` are every entity's and say nothing about this one.
pub fn types_of(schema: &Value) -> Vec<EntityType> {
    let Some(definitions) = schema
        .get("definitions")
        .or_else(|| schema.get("$defs"))
        .and_then(Value::as_object)
    else {
        return Vec::new();
    };
    let text = |value: &Value, key: &str| {
        value
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_owned)
    };
    definitions
        .iter()
        .filter_map(|(class, definition)| {
            let properties = definition.get("properties")?.as_object()?;
            Some(EntityType {
                class: class.clone(),
                description: text(definition, "description"),
                attributes: properties
                    .iter()
                    .filter(|(name, _)| !matches!(name.as_str(), "id" | "type"))
                    .map(|(name, property)| Attribute {
                        name: name.clone(),
                        kind: text(property, "x-ngsi-ld-kind").unwrap_or_else(|| "Property".into()),
                        description: text(property, "description"),
                        unit: property
                            .get("x-unit")
                            .and_then(|unit| text(unit, "ucumCode")),
                    })
                    .collect(),
            })
        })
        .collect()
}

/// `relative` beside the manifest at `manifest` in `root`, or `None` when it leaves the
/// repository: an artifact path is author input.
fn beside(root: &Path, manifest: &Path, relative: &str) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for part in manifest
        .parent()
        .unwrap_or(Path::new(""))
        .join(relative)
        .components()
    {
        match part {
            Component::ParentDir => {
                if !out.pop() {
                    return None;
                }
            }
            Component::Normal(name) => out.push(name),
            Component::CurDir => {}
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(root.join(out))
}

fn language_text(map: Option<&Value>, languages: &[String]) -> Option<String> {
    let map = map?;
    if let Some(plain) = map.as_str() {
        return Some(plain.to_owned());
    }
    let map = map.as_object()?;
    languages
        .iter()
        .map(String::as_str)
        .chain(["en"])
        .find_map(|lang| map.get(lang).and_then(Value::as_str))
        .or_else(|| map.values().find_map(Value::as_str))
        .map(str::to_owned)
}

/// The pages of `project`'s Endpoints over `spaces` (every space when empty), public ones alone
/// when `public_only`. `ckan` is each `CkanInstance` URL of the project, by name.
pub fn pages_of(
    repository: &Repository,
    project: &str,
    spaces: &[String],
    public_only: bool,
    languages: &[String],
    ckan: &BTreeMap<String, String>,
) -> Vec<EndpointPage> {
    // Every published or draft model owned by a space of the project, its types read from the
    // generated JSON Schema beside it. A mirrored copy of a peer's model is not this project's.
    let mut types: BTreeMap<String, Vec<EntityType>> = BTreeMap::new();
    for (id, resource) in repository.iter() {
        if id.kind != "DataModel" || id.namespace.as_deref() != Some(project) {
            continue;
        }
        let Ok(spec) = serde_json::from_value::<DataModelSpec>(resource.manifest.spec.clone())
        else {
            continue;
        };
        let (Some(space), Some(schema)) = (spec.context_space_ref, spec.artifacts.json_schema)
        else {
            continue;
        };
        if spec.lifecycle == DataModelLifecycle::Mirrored {
            continue;
        }
        let Some(path) = beside(repository.root(), &resource.path, &schema) else {
            tracing::warn!(project, model = %id.name, "the model's schema path leaves the repository");
            continue;
        };
        match std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        {
            Some(schema) => types.entry(space).or_default().extend(types_of(&schema)),
            None => {
                tracing::warn!(project, model = %id.name, "the model's JSON Schema is not readable")
            }
        }
    }

    let mut pages = Vec::new();
    for (id, resource) in repository.iter() {
        if id.kind != "Endpoint" || id.namespace.as_deref() != Some(project) {
            continue;
        }
        let Ok(spec) = serde_json::from_value::<EndpointSpec>(resource.manifest.spec.clone())
        else {
            continue;
        };
        let space = spec.context_space_ref.name().to_owned();
        if !spaces.is_empty() && !spaces.contains(&space) {
            continue;
        }
        let audience = spec.audience.to_string();
        if public_only && audience != "public" {
            continue;
        }
        let hidden: HashSet<&str> = spec
            .projection
            .as_ref()
            .map(|p| p.hidden_attributes.iter().map(String::as_str).collect())
            .unwrap_or_default();
        let ckan_dataset = spec
            .publish
            .as_ref()
            .and_then(|publish| publish.ckan.as_ref())
            .and_then(|publication| {
                let base = ckan.get(publication.instance_ref.name())?;
                Some(format!(
                    "{}/dataset/{}",
                    base.trim_end_matches('/'),
                    publication.dataset_name(&id.name)
                ))
            });
        let rest = &resource.manifest.metadata.rest;
        pages.push(EndpointPage {
            name: id.name.clone(),
            title: language_text(rest.get("title"), languages),
            description: language_text(rest.get("description"), languages),
            space: space.clone(),
            slug: spec.slug.to_string(),
            audience,
            ckan_dataset,
            types: types
                .get(&space)
                .map(|held| {
                    held.iter()
                        .map(|t| EntityType {
                            attributes: t
                                .attributes
                                .iter()
                                .filter(|a| !hidden.contains(a.name.as_str()))
                                .cloned()
                                .collect(),
                            ..t.clone()
                        })
                        .collect()
                })
                .unwrap_or_default(),
        });
    }
    pages
}

/// A digest of what the pages are written from: a change to a manifest or a model changes it,
/// and the worker reads the source again (AG-116).
pub fn digest(pages: &[EndpointPage]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(format!("{pages:?}").as_bytes());
    hex::encode(hasher.finalize())
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// The page an Endpoint is indexed as, HTML so it goes through the crawl's extraction; every
/// value escaped. `counts` holds a public Endpoint's entities per type, `counted_at` when.
pub fn page_html(
    page: &EndpointPage,
    address: &str,
    counts: &BTreeMap<String, u64>,
    counted_at: Option<&str>,
) -> String {
    let mut html = String::from("<html><body>");
    let title = page.title.as_deref().unwrap_or(&page.name);
    html.push_str(&format!("<h1>{}</h1>", escape(title)));
    if let Some(description) = &page.description {
        html.push_str(&format!("<p>{}</p>", escape(description)));
    }
    html.push_str(&format!(
        "<p>Endpoint {} of context space {}, audience {}. Address: {}</p>",
        escape(&page.name),
        escape(&page.space),
        escape(&page.audience),
        escape(address)
    ));
    if let Some(dataset) = &page.ckan_dataset {
        html.push_str(&format!("<p>Open data catalogue: {}</p>", escape(dataset)));
    }
    for entity_type in &page.types {
        html.push_str(&format!("<h2>{}</h2>", escape(&entity_type.class)));
        if let Some(description) = &entity_type.description {
            html.push_str(&format!("<p>{}</p>", escape(description)));
        }
        if let Some(count) = counts.get(&entity_type.class) {
            let when = counted_at
                .map(|at| format!(" (counted {})", escape(at)))
                .unwrap_or_default();
            html.push_str(&format!(
                "<p>{count} entities of type {}{when}.</p>",
                escape(&entity_type.class)
            ));
        }
        if !entity_type.attributes.is_empty() {
            html.push_str("<ul>");
            for attribute in &entity_type.attributes {
                let mut item = format!("{} ({})", escape(&attribute.name), escape(&attribute.kind));
                if let Some(unit) = &attribute.unit {
                    item.push_str(&format!(", unit {}", escape(unit)));
                }
                if let Some(description) = &attribute.description {
                    item.push_str(&format!(": {}", escape(description)));
                }
                html.push_str(&format!("<li>{item}</li>"));
            }
            html.push_str("</ul>");
        }
    }
    html.push_str("</body></html>");
    html
}

/// Where a catalogue's counts and citations come from: the gateway inside the cluster, read
/// anonymously, and the public base the Endpoints are addressed under.
#[derive(Debug, Clone)]
pub struct Reader {
    pub http: reqwest::Client,
    /// `JC_ASSISTANT_GATEWAY_URL`; no counts without it.
    pub gateway: Option<Url>,
    /// `JC_ASSISTANT_ENDPOINT_URL`, the public origin `/api/endpoint/{slug}` is served under; a
    /// page with no CKAN dataset has no citation without it and is not indexed.
    pub endpoint_base: Option<Url>,
}

/// The entities of each type a public Endpoint serves, as an anonymous caller counts them. A
/// type whose count does not come back is left out rather than reported as zero.
pub async fn counts(
    http: &reqwest::Client,
    gateway: &Url,
    page: &EndpointPage,
) -> BTreeMap<String, u64> {
    let mut counted = BTreeMap::new();
    if !page.is_public() {
        return counted;
    }
    for entity_type in &page.types {
        let Ok(mut url) = gateway.join(&format!("api/endpoint/{}/ngsi-ld/v1/entities", page.slug))
        else {
            continue;
        };
        url.query_pairs_mut()
            .append_pair("type", &entity_type.class)
            .append_pair("count", "true")
            .append_pair("limit", "0");
        let count = match http.get(url).send().await {
            Ok(answer) if answer.status().is_success() => answer
                .headers()
                .get("NGSILD-Results-Count")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok()),
            Ok(answer) => {
                tracing::info!(endpoint = %page.name, class = %entity_type.class, status = %answer.status(), "not counted");
                None
            }
            Err(err) => {
                tracing::info!(endpoint = %page.name, class = %entity_type.class, %err, "not counted");
                None
            }
        };
        if let Some(count) = count {
            counted.insert(entity_type.class.clone(), count);
        }
    }
    counted
}

/// What one run did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CatalogueReport {
    pub endpoints: usize,
    pub removed: usize,
}

/// Indexes `pages` into the source's site, each as one page whose citation is
/// [`EndpointPage::citation`], and removes the pages of Endpoints no longer listed.
pub async fn sync_catalogue(
    pool: &PgPool,
    reader: &Reader,
    project: &str,
    site_id: i64,
    pages: &[EndpointPage],
    language: Option<&str>,
    sink: &mut impl Sink,
) -> Result<CatalogueReport, Error> {
    let Some(endpoint_base) = &reader.endpoint_base else {
        return Err(Error::Crawl(
            "a catalogue source needs JC_ASSISTANT_ENDPOINT_URL, the public address its pages cite"
                .into(),
        ));
    };
    let now = time::OffsetDateTime::now_utc();
    let counted_at = Some(format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        now.year(),
        u8::from(now.month()),
        now.day(),
        now.hour(),
        now.minute(),
        now.second()
    ));
    let mut listed: BTreeSet<String> = BTreeSet::new();
    for page in pages {
        let url = page.citation(endpoint_base)?;
        // Two Endpoints publishing one dataset share its page; the first written keeps it.
        if !listed.insert(url.clone()) {
            continue;
        }
        let address = endpoint_base
            .join(&format!("api/endpoint/{}", page.slug))
            .map(String::from)
            .unwrap_or_default();
        let counts = match &reader.gateway {
            Some(gateway) => counts(&reader.http, gateway, page).await,
            None => BTreeMap::new(),
        };
        let html = page_html(page, &address, &counts, counted_at.as_deref());
        let hash = hex::encode(Sha256::digest(html.as_bytes()));
        let mut tx = crate::project_scope(pool, project).await?;
        let page_id: i64 = sqlx::query_scalar(
            "INSERT INTO pages (site_id, project, url, depth, status, content_hash, language, included, fetched_at) \
             VALUES ($1, $2, $3, 0, 'fetched', $4, $5, true, now()) \
             ON CONFLICT (site_id, url) DO UPDATE SET status = 'fetched', content_hash = EXCLUDED.content_hash, \
                 language = EXCLUDED.language, fetched_at = now() \
             RETURNING id",
        )
        .bind(site_id)
        .bind(project)
        .bind(&url)
        .bind(&hash)
        .bind(language)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        sink.page(page_id, &url, language, &html).await;
    }
    let listed: Vec<String> = listed.into_iter().collect();
    let mut tx = crate::project_scope(pool, project).await?;
    let removed = sqlx::query("DELETE FROM pages WHERE site_id = $1 AND NOT (url = ANY($2))")
        .bind(site_id)
        .bind(&listed)
        .execute(&mut *tx)
        .await?
        .rows_affected() as usize;
    sqlx::query("UPDATE sites SET last_crawl = now() WHERE id = $1")
        .bind(site_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(CatalogueReport {
        endpoints: listed.len(),
        removed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schema() -> Value {
        json!({ "definitions": {
            "AirQualityObserved": {
                "description": "One air-quality reading of one station.",
                "properties": {
                    "id": { "type": "string" },
                    "type": { "type": "string" },
                    "pm10": { "description": "Particulate matter up to 10 µm.", "x-ngsi-ld-kind": "Property",
                              "x-unit": { "ucumCode": "ug/m3" } },
                    "refDevice": { "x-ngsi-ld-kind": "Relationship" },
                    "location": { "description": "Where.", "x-ngsi-ld-kind": "GeoProperty" }
                }
            },
            "Enum": { "enum": ["a"] }
        }})
    }

    #[test]
    fn a_schema_reads_as_types_with_described_attributes() {
        let types = types_of(&schema());
        assert_eq!(
            types.len(),
            1,
            "a definition without properties is no entity type"
        );
        let air = &types[0];
        assert_eq!(air.class, "AirQualityObserved");
        assert_eq!(
            air.description.as_deref(),
            Some("One air-quality reading of one station.")
        );
        let names: Vec<&str> = air.attributes.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(
            names,
            ["pm10", "refDevice", "location"],
            "id and type are left out"
        );
        let pm10 = &air.attributes[0];
        assert_eq!(pm10.unit.as_deref(), Some("ug/m3"));
        assert_eq!(air.attributes[1].kind, "Relationship");
        assert!(types_of(&json!({})).is_empty());
    }

    fn page(audience: &str) -> EndpointPage {
        EndpointPage {
            name: "public-air".into(),
            title: Some("Air <quality>".into()),
            description: None,
            space: "ovzdusie".into(),
            slug: "k7m2qz4tv6xh3n5jb2ryd3wcfa".into(),
            audience: audience.into(),
            ckan_dataset: None,
            types: types_of(&schema()),
        }
    }

    #[test]
    fn a_page_cites_its_dataset_or_its_own_address() {
        let base = Url::parse("https://data.example.org/").expect("url");
        let mut endpoint = page("public");
        assert_eq!(
            endpoint.citation(&base).expect("citation"),
            "https://data.example.org/api/endpoint/k7m2qz4tv6xh3n5jb2ryd3wcfa/schema/index.json"
        );
        endpoint.ckan_dataset = Some("https://ckan.example.org/dataset/public-air".into());
        assert_eq!(
            endpoint.citation(&base).expect("citation"),
            "https://ckan.example.org/dataset/public-air"
        );
    }

    #[test]
    fn a_page_names_the_types_their_attributes_and_the_counts_escaped() {
        let counts = BTreeMap::from([("AirQualityObserved".to_owned(), 14)]);
        let html = page_html(
            &page("public"),
            "https://x/api/endpoint/k",
            &counts,
            Some("2026-10-07T20:00:00Z"),
        );
        assert!(html.contains("<h1>Air &lt;quality&gt;</h1>"), "{html}");
        assert!(html.contains("context space ovzdusie, audience public"));
        assert!(html.contains("<h2>AirQualityObserved</h2>"));
        assert!(
            html.contains("14 entities of type AirQualityObserved (counted 2026-10-07T20:00:00Z).")
        );
        assert!(
            html.contains("<li>pm10 (Property), unit ug/m3: Particulate matter up to 10 µm.</li>")
        );
        let uncounted = page_html(&page("organization"), "a", &BTreeMap::new(), None);
        assert!(
            !uncounted.contains("entities of type"),
            "no count, no sentence"
        );
    }

    #[test]
    fn a_path_that_leaves_the_repository_is_refused() {
        let root = Path::new("/repo");
        assert_eq!(
            beside(
                root,
                Path::new("projects/p/spaces/s/model.yaml"),
                "./m.v1.schema.json"
            ),
            Some(PathBuf::from("/repo/projects/p/spaces/s/m.v1.schema.json"))
        );
        assert_eq!(
            beside(root, Path::new("model.yaml"), "../../etc/passwd"),
            None
        );
        assert_eq!(beside(root, Path::new("a/model.yaml"), "/etc/passwd"), None);
    }

    #[test]
    fn the_digest_follows_what_the_pages_are_written_from() {
        let one = digest(&[page("public")]);
        assert_eq!(one, digest(&[page("public")]));
        assert_ne!(one, digest(&[page("organization")]));
    }
}
