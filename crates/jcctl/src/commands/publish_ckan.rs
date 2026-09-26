//! `jcctl publish ckan --repo-dir <path> --project <p> --host <gateway host>` (T-0487,
//! EP-62…EP-67, CC-18).
//!
//! The command walks the configuration repository the way `apply` does, takes every
//! Endpoint of one project that declares `spec.publish.ckan`, resolves the `CkanInstance`
//! it names and the API token behind the instance's `apiTokenRef`, reads the Endpoint's
//! own DCAT-AP record from the gateway and hands all of that to
//! [`crate::publish::ckan::publish`]. An Endpoint that also declares a DataStore mirror gets
//! its rows through the same gateway, as any consumer would (EP-65, EP-66).
//!
//! Nothing here is authored a second time: the dataset is the record, the resources are
//! the enabled representations, and a second run over an unchanged repository makes no
//! writing call to the catalogue (CC-18). The token is read here and handed to the HTTP
//! client; it is never printed, and never part of an error (EP-67).

use crate::loader::{LoadError, RawManifest, Repository, ResourceId};
use crate::publish::ckan::{self, CkanApi, Outcome, PublishError, Settings};
use crate::publish::ckan_datastore::{self as datastore, MirrorError};
use crate::secrets::{identities_from_file, SecretStore, SecretValue};
use jc_core::envelope::{Kind, Ref};
use jc_core::i18n::Text;
use jc_core::kinds::ckan::{CkanInstanceSpec, CkanPublication};
use jc_core::kinds::{ContextSpaceSpec, EndpointSpec, Representation};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::Path;
use std::time::Duration;

/// The environment variable `sops` reads its age key file from, read here for the same
/// purpose when `--age-key-file` is absent.
pub const AGE_KEY_FILE_ENV: &str = "SOPS_AGE_KEY_FILE";

/// How long one read of the gateway may take: a `file.csv` of a whole space is the slow one.
const FETCH_TIMEOUT: Duration = Duration::from_secs(120);

/// One Endpoint the repository publishes, with the instance it publishes to.
#[derive(Debug, Clone, PartialEq)]
pub struct Target {
    /// The Endpoint's identity in the repository.
    pub id: ResourceId,
    /// The Endpoint manifest, as [`ckan::publish`] takes it.
    pub manifest: RawManifest,
    /// The Endpoint's slug, which is where the gateway answers for it.
    pub slug: String,
    /// The `spec.publish.ckan` block.
    pub publication: CkanPublication,
    /// The name of the `CkanInstance` the block names.
    pub instance_name: String,
    /// The instance itself.
    pub instance: CkanInstanceSpec,
    /// The `defaultLocale` of the Endpoint's space: the language its dataset is written in
    /// (EP-63). `None` when the space names none or is not in the repository.
    pub language: Option<String>,
    /// The instance's `metadata.title` in that language: what an organization this run has
    /// to create is called, before the installation's branding name.
    pub instance_title: Option<String>,
}

impl Target {
    /// The CKAN dataset name this Endpoint publishes as.
    pub fn dataset_name(&self) -> &str {
        self.publication.dataset_name(&self.id.name)
    }

    /// `settings` for this Endpoint: its space's language, and its instance's title for an
    /// organization CKAN does not have yet.
    fn settings(&self, settings: &Settings) -> Settings {
        let mut own = settings.clone();
        if let Some(language) = &self.language {
            own = own.in_language(language.clone());
        }
        if let Some(title) = &self.instance_title {
            own = own.titled(title.clone());
        }
        own
    }
}

/// What one run did to the DataStore mirror of one Endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mirror {
    /// What happened to the table.
    pub table: datastore::Outcome,
    /// How many rows were written into it.
    pub rows: usize,
}

/// One printed line: what happened to one Endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    /// The Endpoint.
    pub endpoint: ResourceId,
    /// The dataset name in CKAN.
    pub dataset: String,
    /// What happened to the dataset.
    pub outcome: Outcome,
    /// What happened to the mirror, when the Endpoint declares one.
    pub mirror: Option<Mirror>,
}

impl fmt::Display for Line {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}: dataset {} {}",
            self.endpoint,
            self.dataset,
            outcome_word(self.outcome)
        )?;
        if let Some(mirror) = &self.mirror {
            write!(
                f,
                ", DataStore {} ({} rows)",
                table_word(mirror.table),
                mirror.rows
            )?;
        }
        Ok(())
    }
}

/// Why the command could not do its work for one Endpoint, or at all.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The repository could not be loaded.
    #[error(transparent)]
    Load(#[from] LoadError),
    /// An Endpoint's spec does not parse.
    #[error("{endpoint}: endpoint spec: {message}")]
    Spec {
        /// The Endpoint.
        endpoint: Box<ResourceId>,
        /// What serde said.
        message: String,
    },
    /// The publication names a `CkanInstance` the repository does not hold.
    #[error(
        "{endpoint}: publish.ckan.instanceRef names {instance}, which is not in the repository"
    )]
    UnknownInstance {
        /// The Endpoint. Boxed, with the instance: two identities side by side would make
        /// every `Result` of this module carry them.
        endpoint: Box<ResourceId>,
        /// The instance it named.
        instance: Box<ResourceId>,
    },
    /// A `CkanInstance` spec does not parse or validate.
    #[error("{instance}: {message}")]
    Instance {
        /// The instance.
        instance: Box<ResourceId>,
        /// What went wrong.
        message: String,
    },
    /// The API token could not be resolved. Carries no plaintext.
    #[error("api token of CkanInstance {instance}: {message}")]
    Token {
        /// The instance whose token was asked for.
        instance: String,
        /// Why it could not be read.
        message: String,
    },
    /// The gateway did not answer a read as expected.
    #[error("GET {url}: {message}")]
    Gateway {
        /// What was read.
        url: String,
        /// The status and what came back, or why nothing did.
        message: String,
    },
    /// The rows the mirror is filled from cannot be read.
    #[error("DataStore rows: {0}")]
    Rows(String),
    /// The publisher refused or CKAN did.
    #[error(transparent)]
    Publish(#[from] PublishError),
    /// The mirror refused or CKAN did.
    #[error(transparent)]
    Mirror(#[from] MirrorError),
}

/// Every Endpoint of `project` that declares `spec.publish.ckan`, with its instance
/// resolved, in repository order (EP-62).
///
/// The instance is looked up in the project the Endpoint is in, unless the reference
/// names a namespace itself. An Endpoint that names no publication is not a target and
/// not an error; one whose spec or instance is broken stops the walk, because a run that
/// silently skips a misdeclared Endpoint would report a catalogue that is not converged.
pub fn targets(repo: &Repository, project: &str) -> Result<Vec<Target>, Error> {
    let mut targets = Vec::new();
    for (id, resource) in repo.iter() {
        if id.kind != "Endpoint" || id.namespace.as_deref() != Some(project) {
            continue;
        }
        let spec: EndpointSpec =
            serde_json::from_value(resource.manifest.spec.clone()).map_err(|e| Error::Spec {
                endpoint: Box::new(id.clone()),
                message: e.to_string(),
            })?;
        let Some(publication) = spec.publish.as_ref().and_then(|p| p.ckan.clone()) else {
            continue;
        };
        let namespace = match &publication.instance_ref {
            Ref::Typed(typed) => typed
                .namespace
                .clone()
                .unwrap_or_else(|| project.to_owned()),
            Ref::Name(_) => project.to_owned(),
        };
        let instance_id = ResourceId::new(
            id.group.clone(),
            CkanInstanceSpec::KIND,
            Some(namespace),
            publication.instance_ref.name(),
        );
        let Some(instance_manifest) = repo.get(&instance_id) else {
            return Err(Error::UnknownInstance {
                endpoint: Box::new(id.clone()),
                instance: Box::new(instance_id),
            });
        };
        let instance: CkanInstanceSpec =
            serde_json::from_value(instance_manifest.manifest.spec.clone()).map_err(|e| {
                Error::Instance {
                    instance: Box::new(instance_id.clone()),
                    message: e.to_string(),
                }
            })?;
        instance.validate().map_err(|e| Error::Instance {
            instance: Box::new(instance_id.clone()),
            message: e.to_string(),
        })?;
        let language = space_language(repo, id, &spec);
        let instance_title = title_in(
            &instance_manifest.manifest.metadata.rest,
            language.as_deref(),
        );
        targets.push(Target {
            id: id.clone(),
            manifest: resource.manifest.clone(),
            slug: spec.slug.as_str().to_owned(),
            publication,
            instance_name: instance_id.name,
            instance,
            language,
            instance_title,
        });
    }
    Ok(targets)
}

/// The `defaultLocale` of the space an Endpoint serves, looked up in the Endpoint's project.
fn space_language(repo: &Repository, endpoint: &ResourceId, spec: &EndpointSpec) -> Option<String> {
    let namespace = match &spec.context_space_ref {
        Ref::Typed(typed) => typed
            .namespace
            .clone()
            .or_else(|| endpoint.namespace.clone()),
        Ref::Name(_) => endpoint.namespace.clone(),
    };
    let space = ResourceId::new(
        endpoint.group.clone(),
        ContextSpaceSpec::KIND,
        namespace,
        spec.context_space_ref.name(),
    );
    let space: ContextSpaceSpec =
        serde_json::from_value(repo.get(&space)?.manifest.spec.clone()).ok()?;
    space
        .default_locale
        .filter(|locale| !locale.trim().is_empty())
}

/// A manifest's `metadata.title`, a plain string or the legacy language map, in `language`
/// when the map has it, else English, else its first entry (PF-28).
fn title_in(metadata: &serde_json::Map<String, Value>, language: Option<&str>) -> Option<String> {
    let title: Text = serde_json::from_value(metadata.get("title")?.clone()).ok()?;
    let preferred: Vec<String> = language.map(str::to_owned).into_iter().collect();
    let text = title.resolve(&preferred, "en").trim();
    (!text.is_empty()).then(|| text.to_owned())
}

/// Where the API token of an instance is read from, in the order tried.
#[derive(Debug, Clone, Copy, Default)]
pub struct TokenSource<'a> {
    /// `--api-token-env`: the environment variable holding the token, for a machine
    /// without the repository's age key.
    pub env: Option<&'a str>,
    /// `--age-key-file`: the age identity that decrypts the repository's secrets; the
    /// value of [`AGE_KEY_FILE_ENV`] when absent.
    pub age_key_file: Option<&'a Path>,
}

/// The API token behind `spec.apiTokenRef`, resolved the way the reconciler resolves a
/// secret reference (CC-06, EP-67).
///
/// Three sources, first hit wins: the variable named on the command line; the variable
/// the reference itself names as `envVar`, which is how a token is injected into a Job;
/// and the repository's SOPS-encrypted secrets, decrypted with the age key. The token
/// is never in a manifest, and this function never echoes it.
pub fn token(
    instance: &CkanInstanceSpec,
    repo_root: &Path,
    source: TokenSource<'_>,
) -> Result<SecretValue, Error> {
    let reference = &instance.api_token_ref;
    let fail = |message: String| Error::Token {
        instance: reference.name.clone(),
        message,
    };

    if let Some(variable) = source.env {
        return SecretValue::from_env(variable)
            .ok_or_else(|| fail(format!("{variable} is not set or empty")));
    }
    if let Some(value) = reference.env_var.as_deref().and_then(SecretValue::from_env) {
        return Ok(value);
    }

    let key_file = match source.age_key_file {
        Some(path) => path.to_path_buf(),
        None => match std::env::var_os(AGE_KEY_FILE_ENV) {
            Some(path) => path.into(),
            None => {
                return Err(fail(format!(
                    "no way to read it: pass --api-token-env <VAR>, or --age-key-file / \
                     {AGE_KEY_FILE_ENV} to decrypt the repository's secrets"
                )))
            }
        },
    };
    let identities = identities_from_file(&key_file).map_err(|e| fail(e.to_string()))?;
    let store = SecretStore::load_dir(repo_root, &identities).map_err(|e| fail(e.to_string()))?;
    let value = store.resolve(reference).map_err(|e| fail(e.to_string()))?;
    if value.is_empty() {
        return Err(fail("the secret is empty".to_owned()));
    }
    Ok(SecretValue::new(value.expose().to_owned()))
}

/// One `GET` of the gateway, as an anonymous consumer (EP-66).
///
/// `accept` picks the serialization; anything but a `2xx` is an error naming the status
/// and the start of what came back, which for a problem document is the detail.
pub fn fetch(url: &str, accept: &str) -> Result<String, Error> {
    let gateway = |message: String| Error::Gateway {
        url: url.to_owned(),
        message,
    };
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(FETCH_TIMEOUT)
        .user_agent(ckan::GENERATOR)
        .build()
        .map_err(|e| gateway(format!("HTTP client: {e}")))?;
    let response = client
        .get(url)
        .header("Accept", accept)
        .send()
        .map_err(|e| gateway(e.to_string()))?;
    let status = response.status();
    let text = response.text().map_err(|e| gateway(e.to_string()))?;
    if !status.is_success() {
        let detail: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
        let shown: String = detail.chars().take(200).collect();
        return Err(gateway(format!("{} {shown}", status.as_u16())));
    }
    Ok(text)
}

/// The DCAT-AP record of one target, read from the gateway (EP-27, EP-63).
pub fn record(target: &Target, settings: &Settings) -> Result<Value, Error> {
    let url = format!("{}/", settings.endpoint_url(&target.slug));
    let text = fetch(&url, "application/ld+json")?;
    serde_json::from_str(&text).map_err(|e| Error::Gateway {
        url,
        message: format!("the record is not JSON: {e}"),
    })
}

/// What one target's mirror is filled from: the Endpoint's tabular answer, and the JSON Schema
/// of its models that says which columns each entity type's table carries (EP-65, T-3012).
#[derive(Debug, Clone, PartialEq)]
pub struct Rows {
    /// The CSV the gateway answers for the Endpoint.
    pub csv: String,
    /// The Endpoint's `model.schema.json`, narrowed to the same anonymous grant as the rows;
    /// `None` leaves every table to the columns its rows fill.
    pub schema: Option<Value>,
}

/// The rows of one target's mirror, read through the representation it declares, and the
/// schema of its models (EP-65, T-3012).
pub fn rows(target: &Target, settings: &Settings) -> Result<Option<Rows>, Error> {
    let Some(mirror) = &target.publication.datastore else {
        return Ok(None);
    };
    let (path, accept) = representation_file(mirror.representation)?;
    let base = settings.endpoint_url(&target.slug);
    let csv = fetch(&format!("{base}/{path}"), accept)?;
    let url = format!("{base}/schema/v1/model.schema.json");
    let text = fetch(&url, "application/schema+json")?;
    let schema = serde_json::from_str(&text).map_err(|e| Error::Gateway {
        url,
        message: format!("the schema is not JSON: {e}"),
    })?;
    Ok(Some(Rows {
        csv,
        schema: Some(schema),
    }))
}

/// The file the mirror's rows are read from.
///
/// Only `csv` is read: `xlsx` is a binary the gateway builds for a spreadsheet, and the
/// JSON file is not a tabular projection. The manifest validator admits all three, so
/// this is where the other two are refused, with the fix named.
fn representation_file(
    representation: Representation,
) -> Result<(&'static str, &'static str), Error> {
    match representation {
        Representation::Csv => Ok(("file.csv", "text/csv")),
        other => Err(Error::Rows(format!(
            "jcctl fills the DataStore through file.csv; publish.ckan.datastore.representation \
             is {}, declare csv",
            other.as_str()
        ))),
    }
}

/// Publishes one Endpoint: the dataset, and the mirror when it declares one (EP-62, EP-65).
///
/// `rows` is the tabular answer the mirror is filled from and the schema of its models, `None`
/// when the Endpoint declares no mirror. The mirror is a full reload on every run: every row the
/// Endpoint answers is upserted into its entity type's table, so each table equals the answer
/// for every entity it carries. A dataset that already matches makes no writing call (CC-18).
pub fn publish_one(
    api: &mut impl CkanApi,
    target: &Target,
    record: &Value,
    rows: Option<&Rows>,
    settings: &Settings,
) -> Result<Line, Error> {
    let settings = target.settings(settings);
    let outcome = ckan::publish(api, &target.manifest, &target.instance, record, &settings)?;
    let mirror = match (&target.publication.datastore, rows) {
        (Some(_), Some(rows)) => Some(mirror(api, target, rows)?),
        (Some(_), None) => {
            return Err(Error::Rows(
                "the endpoint declares a DataStore mirror and no rows were read".to_owned(),
            ))
        }
        (None, _) => None,
    };
    Ok(Line {
        endpoint: target.id.clone(),
        dataset: target.dataset_name().to_owned(),
        outcome,
        mirror,
    })
}

/// Withdraws one Endpoint's dataset, and drops its mirror first (EP-62, CC-19).
///
/// Withdrawing what is not there succeeds, so a run can be repeated after a partial
/// failure.
pub fn withdraw_one(api: &mut impl CkanApi, target: &Target) -> Result<Line, Error> {
    let name = target.dataset_name().to_owned();
    let mirror = match &target.publication.datastore {
        Some(_) => {
            let (_, tables) = live_tables(api, &name)?;
            let mut table = datastore::Outcome::Unchanged;
            for resource_id in tables.values() {
                if datastore::drop_table(api, resource_id)? == datastore::Outcome::NotMirrored {
                    table = datastore::Outcome::NotMirrored;
                }
            }
            Some(Mirror { table, rows: 0 })
        }
        None => None,
    };
    let outcome = ckan::withdraw(api, &name)?;
    Ok(Line {
        endpoint: target.id.clone(),
        dataset: name,
        outcome,
        mirror,
    })
}

/// Where one column of an entity type's table takes its cells from (T-3012).
enum Source {
    /// The CSV column at this index.
    Cell(usize),
    /// Nothing yet: a column only the model brought, which a row fills once the data has it.
    Empty,
    /// One geometry as a GeoJSON string, built from the CSV columns of its `value.type` and of
    /// its coordinates, whole (`value.coordinates`) or spread by index (`value.coordinates[0]`).
    GeoJson {
        kind: usize,
        whole: Option<usize>,
        indexed: Vec<usize>,
    },
}

/// Creates or extends one table per entity type and reloads each from `rows` (EP-65, T-3012).
///
/// An Endpoint that serves several entity types gets a table per type, named by the type, with
/// that type's rows only: one table of every type made each row a line of mostly empty cells of
/// other types' attributes. A table's columns are the ones its rows fill, every column the
/// type's model declares (an attribute no entity carries today still has its column), and a
/// GeoJSON column per geometry. The table of a type the Endpoint no longer answers, and the one
/// shared table a run before T-3012 wrote, are removed with their resource.
///
/// The table is held once, as the text cells of the CSV, and every other shape of a row is
/// built one batch at a time and dropped with it: a typed copy, a copy keeping the text columns
/// and the records of every row at once came to 27 times the CSV, and the Portal runs this for
/// every mirrored table on every pass (T-2968). Every row is checked before the catalogue is
/// written, so a table with one bad row still changes nothing.
fn mirror(api: &mut impl CkanApi, target: &Target, rows: &Rows) -> Result<Mirror, Error> {
    let (columns, raw) = csv_table(&rows.csv)?;
    let id = datastore::id_column(&columns)?;
    let kind = columns
        .iter()
        .position(|column| column == "type")
        .ok_or_else(|| {
            Error::Rows(
                "the CSV has no 'type' column, so a row cannot go into its entity type's table"
                    .to_owned(),
            )
        })?;
    let mut by_type: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for (index, row) in raw.iter().enumerate() {
        if row.len() != columns.len() {
            return Err(MirrorError::RaggedRow {
                row: index,
                cells: row.len(),
                columns: columns.len(),
            }
            .into());
        }
        for (column, what) in [(id, "id"), (kind, "type")] {
            if row[column].is_empty() {
                return Err(Error::Rows(format!("row {index} has no {what}")));
            }
        }
        by_type.entry(row[kind].as_str()).or_default().push(index);
    }
    let languages = languages(&columns, target.language.as_deref());

    let (package_id, live) = live_tables(api, target.dataset_name())?;
    let mut table = datastore::Outcome::Unchanged;
    let mut written = 0;
    let mut refused_view = None;
    for (entity_type, indices) in &by_type {
        let definition = rows
            .schema
            .as_ref()
            .and_then(|schema| datastore::definition(schema, entity_type));
        let (names, sources) = layout(&columns, &raw, indices, [id, kind], definition, &languages);
        let fields = datastore::table_fields(&names, definition, |index| match &sources[index] {
            Source::Cell(column) => Some(datastore::observed_kind(
                indices.iter().map(|&row| typed_cell(&raw[row][*column])),
            )),
            Source::Empty => None,
            Source::GeoJson { .. } => Some("text".to_owned()),
        });
        // A column CKAN will hold as text keeps the cell as it was written: `42` in a text
        // column is the text "42", not a number the database would refuse.
        let text_columns: Vec<bool> = fields
            .iter()
            .map(|field| field.get("type") == Some(&Value::String("text".to_owned())))
            .collect();
        let values = |row: &[String]| -> Vec<Value> {
            sources
                .iter()
                .zip(&text_columns)
                .map(|(source, text)| match source {
                    Source::Cell(column) => {
                        let cell = &row[*column];
                        if *text && !cell.is_empty() {
                            Value::String(cell.clone())
                        } else {
                            typed_cell(cell)
                        }
                    }
                    Source::Empty => Value::Null,
                    Source::GeoJson {
                        kind,
                        whole,
                        indexed,
                    } => geojson(row, *kind, *whole, indexed),
                })
                .collect()
        };

        let resource = live.get(*entity_type).map_or(*entity_type, String::as_str);
        let (resource_id, outcome) = datastore::ensure(api, &package_id, resource, &fields)?;
        table = match (table, outcome) {
            (datastore::Outcome::Created, _) | (_, datastore::Outcome::Created) => {
                datastore::Outcome::Created
            }
            (datastore::Outcome::Extended, _) | (_, datastore::Outcome::Extended) => {
                datastore::Outcome::Extended
            }
            _ => datastore::Outcome::Unchanged,
        };
        // The grid is asked for as soon as the table exists, and its refusal is reported after
        // the rows: a sync that fails part-way or a pass cut off on a large table still leaves
        // the view (T-2931), and a catalogue that refuses the view still holds today's data.
        if let Err(error) = datastore::ensure_view(api, &resource_id) {
            refused_view.get_or_insert(error);
        }
        for chunk in indices.chunks(datastore::UPSERT_BATCH) {
            let batch = chunk
                .iter()
                .map(|&row| datastore::record(&names, row, values(&raw[row])))
                .collect::<Result<Vec<_>, _>>()?;
            written += batch.len();
            datastore::upsert(api, &resource_id, batch)?;
        }
    }

    // An answer without a row says nothing about which types are gone, so it removes nothing.
    if !by_type.is_empty() {
        for (name, resource_id) in &live {
            if by_type.contains_key(name.as_str()) {
                continue;
            }
            datastore::drop_table(api, resource_id)?;
            api.action("resource_delete", &json!({ "id": resource_id }))
                .map_err(MirrorError::from)?;
        }
    }
    if let Some(error) = refused_view {
        return Err(error.into());
    }
    Ok(Mirror {
        table,
        rows: written,
    })
}

/// The columns of one entity type's table and where each takes its cells from (T-3012).
///
/// The CSV columns this type's rows fill come first, in the gateway's order, with `id` and
/// `type` always; then every column the type's model declares that those do not carry; then a
/// GeoJSON column per geometry the table holds.
fn layout(
    columns: &[String],
    raw: &[Vec<String>],
    indices: &[usize],
    always: [usize; 2],
    definition: Option<&Value>,
    languages: &[String],
) -> (Vec<String>, Vec<Source>) {
    let mut names = Vec::new();
    let mut sources = Vec::new();
    for (column, name) in columns.iter().enumerate() {
        if always.contains(&column) || indices.iter().any(|&row| !raw[row][column].is_empty()) {
            names.push(name.clone());
            sources.push(Source::Cell(column));
        }
    }
    for column in definition.map_or_else(Vec::new, |definition| {
        datastore::model_columns(definition, languages)
    }) {
        if !datastore::covered(&names, &column) {
            names.push(column);
            sources.push(Source::Empty);
        }
    }
    let geometries: Vec<String> = names
        .iter()
        .filter_map(|name| name.strip_suffix(".value.type"))
        .filter(|attribute| datastore::covered(&names, &format!("{attribute}.value.coordinates")))
        .map(str::to_owned)
        .collect();
    for attribute in geometries {
        let name = format!("{attribute}.geojson");
        if names.contains(&name) {
            continue;
        }
        let at = |wanted: &str| columns.iter().position(|column| column == wanted);
        let whole = format!("{attribute}.value.coordinates");
        let mut indexed: Vec<(usize, usize)> = columns
            .iter()
            .enumerate()
            .filter_map(|(column, name)| {
                let index = name
                    .strip_prefix(&whole)?
                    .strip_prefix('[')?
                    .strip_suffix(']')?;
                Some((index.parse().ok()?, column))
            })
            .collect();
        indexed.sort_unstable();
        let source = match at(&format!("{attribute}.value.type")) {
            Some(kind) => Source::GeoJson {
                kind,
                whole: at(&whole),
                indexed: indexed.into_iter().map(|(_, column)| column).collect(),
            },
            None => Source::Empty,
        };
        names.push(name);
        sources.push(source);
    }
    (names, sources)
}

/// One geometry as a GeoJSON string, or null when the row carries none (T-3012).
fn geojson(row: &[String], kind: usize, whole: Option<usize>, indexed: &[usize]) -> Value {
    let Some(kind) = row.get(kind).filter(|kind| !kind.is_empty()) else {
        return Value::Null;
    };
    let coordinates = match whole
        .and_then(|column| row.get(column))
        .filter(|c| !c.is_empty())
    {
        Some(text) => typed_cell(text),
        None => Value::Array(
            indexed
                .iter()
                .map_while(|&column| {
                    row.get(column)
                        .filter(|c| !c.is_empty())
                        .map(|c| typed_cell(c))
                })
                .collect(),
        ),
    };
    if coordinates.as_array().is_some_and(Vec::is_empty) || coordinates.is_null() {
        return Value::Null;
    }
    Value::String(json!({ "type": kind, "coordinates": coordinates }).to_string())
}

/// The languages a LanguageProperty's columns are spread over: every language the answer
/// carries, and the space's own (EP-63).
fn languages(columns: &[String], space: Option<&str>) -> Vec<String> {
    columns
        .iter()
        .filter_map(|column| {
            column
                .split_once(".languageMap.")
                .map(|(_, language)| language)
        })
        .chain(space)
        .map(str::to_owned)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// The live dataset's id and its DataStore resources by name, when the dataset exists.
///
/// CKAN addresses a resource by id only, and the id was minted when the table was
/// created; the dataset is where a later run finds it again, by the name it was given.
fn live_tables(
    api: &impl CkanApi,
    dataset: &str,
) -> Result<(String, BTreeMap<String, String>), Error> {
    let Some(live) = api
        .show("package_show", dataset)
        .map_err(PublishError::from)?
    else {
        return Ok((dataset.to_owned(), BTreeMap::new()));
    };
    let package_id = live
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or(dataset)
        .to_owned();
    let tables = live
        .get("resources")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|resource| resource.get("url_type").and_then(Value::as_str) == Some("datastore"))
        .filter_map(|resource| {
            Some((
                resource.get("name")?.as_str()?.to_owned(),
                resource.get("id")?.as_str()?.to_owned(),
            ))
        })
        .collect();
    Ok((package_id, tables))
}

/// The header and the rows of one CSV file as the gateway writes it (RFC 4180): quoted
/// cells may carry commas, quotes doubled, and line breaks; records end in CRLF or LF.
///
/// A blank line is skipped rather than read as an empty record. A ragged row is left to
/// [`datastore::records`], which names the row.
pub fn csv_table(text: &str) -> Result<(Vec<String>, Vec<Vec<String>>), Error> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut records: Vec<Vec<String>> = Vec::new();
    let mut row: Vec<String> = Vec::new();
    let mut cell = String::new();
    let mut quoted = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if quoted {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    chars.next();
                    cell.push('"');
                } else {
                    quoted = false;
                }
            } else {
                cell.push(c);
            }
            continue;
        }
        match c {
            '"' if cell.is_empty() => quoted = true,
            ',' => row.push(std::mem::take(&mut cell)),
            '\r' if chars.peek() == Some(&'\n') => {}
            '\n' => {
                row.push(std::mem::take(&mut cell));
                records.push(std::mem::take(&mut row));
            }
            other => cell.push(other),
        }
    }
    if quoted {
        return Err(Error::Rows("the CSV ends inside a quoted cell".to_owned()));
    }
    if !cell.is_empty() || !row.is_empty() {
        row.push(cell);
        records.push(row);
    }
    records.retain(|record| !(record.len() == 1 && record[0].is_empty()));

    let mut records = records.into_iter();
    let columns = records
        .next()
        .ok_or_else(|| Error::Rows("the CSV has no header".to_owned()))?;
    let unique: BTreeSet<&str> = columns.iter().map(String::as_str).collect();
    if unique.len() != columns.len() || columns.iter().any(String::is_empty) {
        return Err(Error::Rows(
            "the CSV header repeats a column or names an empty one".to_owned(),
        ));
    }
    Ok((columns, records.collect()))
}

/// The value a CSV cell was written from, as far as the text says: the gateway writes
/// a number, a boolean or a structure as its JSON and a string as it stands, and an
/// empty cell for null (EP-44). Anything that is not JSON is the string it is.
pub fn typed_cell(cell: &str) -> Value {
    if cell.is_empty() {
        return Value::Null;
    }
    let looks_like_json = matches!(cell.as_bytes()[0], b'{' | b'[' | b'-' | b'0'..=b'9')
        || cell == "true"
        || cell == "false";
    if looks_like_json {
        if let Ok(value) = serde_json::from_str::<Value>(cell) {
            return value;
        }
    }
    Value::String(cell.to_owned())
}

fn outcome_word(outcome: Outcome) -> &'static str {
    match outcome {
        Outcome::NotPublished => "not published",
        Outcome::Created => "created",
        Outcome::Updated => "updated",
        Outcome::Unchanged => "unchanged",
        Outcome::Withdrawn => "withdrawn",
    }
}

fn table_word(outcome: datastore::Outcome) -> &'static str {
    match outcome {
        datastore::Outcome::NotMirrored => "dropped",
        datastore::Outcome::Created => "created",
        datastore::Outcome::Extended => "extended",
        datastore::Outcome::Unchanged => "unchanged",
    }
}
