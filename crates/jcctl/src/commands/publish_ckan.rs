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
use jc_core::kinds::ckan::{CkanInstanceSpec, CkanPublication};
use jc_core::kinds::{EndpointSpec, Representation};
use serde_json::Value;
use std::collections::BTreeSet;
use std::fmt;
use std::path::Path;
use std::time::Duration;

/// The name of the DataStore resource inside the dataset, which is how a later run finds
/// the table it created (EP-65).
pub const DATASTORE_RESOURCE: &str = "DataStore";

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
}

impl Target {
    /// The CKAN dataset name this Endpoint publishes as.
    pub fn dataset_name(&self) -> &str {
        self.publication.dataset_name(&self.id.name)
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
        let Some(instance) = repo.get(&instance_id) else {
            return Err(Error::UnknownInstance {
                endpoint: Box::new(id.clone()),
                instance: Box::new(instance_id),
            });
        };
        let instance: CkanInstanceSpec = serde_json::from_value(instance.manifest.spec.clone())
            .map_err(|e| Error::Instance {
                instance: Box::new(instance_id.clone()),
                message: e.to_string(),
            })?;
        instance.validate().map_err(|e| Error::Instance {
            instance: Box::new(instance_id.clone()),
            message: e.to_string(),
        })?;
        targets.push(Target {
            id: id.clone(),
            manifest: resource.manifest.clone(),
            slug: spec.slug.as_str().to_owned(),
            publication,
            instance_name: instance_id.name,
            instance,
        });
    }
    Ok(targets)
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

/// The rows of one target's mirror, read through the representation it declares (EP-65).
pub fn rows(target: &Target, settings: &Settings) -> Result<Option<String>, Error> {
    let Some(mirror) = &target.publication.datastore else {
        return Ok(None);
    };
    let (path, accept) = representation_file(mirror.representation)?;
    let url = format!("{}/{path}", settings.endpoint_url(&target.slug));
    fetch(&url, accept).map(Some)
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
/// `rows` is the tabular answer the mirror is filled from, `None` when the Endpoint
/// declares no mirror. The mirror is a full reload on every run: every row the Endpoint
/// answers is upserted, so the table equals the answer for every entity it carries. A
/// dataset that already matches makes no writing call (CC-18).
pub fn publish_one(
    api: &mut impl CkanApi,
    target: &Target,
    record: &Value,
    rows: Option<&str>,
    settings: &Settings,
) -> Result<Line, Error> {
    let outcome = ckan::publish(api, &target.manifest, &target.instance, record, settings)?;
    let mirror = match (&target.publication.datastore, rows) {
        (Some(_), Some(text)) => Some(mirror(api, target, text)?),
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
            let (_, resource) = live_table(api, &name)?;
            let table = match resource {
                Some(id) => datastore::drop_table(api, &id)?,
                None => datastore::Outcome::Unchanged,
            };
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

/// Creates or extends the table and reloads it from `text` (EP-65).
fn mirror(api: &mut impl CkanApi, target: &Target, text: &str) -> Result<Mirror, Error> {
    let (columns, raw) = csv_table(text)?;
    let typed: Vec<Vec<Value>> = raw
        .iter()
        .map(|row| row.iter().map(|cell| typed_cell(cell)).collect())
        .collect();
    let fields = datastore::fields(&columns, &typed, &[]);
    // A column CKAN will hold as text keeps the cell as it was written: `42` in a text
    // column is the text "42", not a number the database would refuse.
    let text_columns: Vec<bool> = fields
        .iter()
        .map(|field| field.get("type") == Some(&Value::String("text".to_owned())))
        .collect();
    let rows: Vec<Vec<Value>> = raw
        .iter()
        .zip(&typed)
        .map(|(raw_row, typed_row)| {
            raw_row
                .iter()
                .zip(typed_row)
                .enumerate()
                .map(|(index, (raw_cell, typed_cell))| {
                    if text_columns.get(index).copied().unwrap_or(true) && !raw_cell.is_empty() {
                        Value::String(raw_cell.clone())
                    } else {
                        typed_cell.clone()
                    }
                })
                .collect()
        })
        .collect();
    let records = datastore::records(&columns, &rows)?;

    let name = target.dataset_name();
    let (package_id, resource) = live_table(api, name)?;
    let (resource_id, table) = datastore::ensure(
        api,
        &package_id,
        resource.as_deref().unwrap_or(DATASTORE_RESOURCE),
        &fields,
    )?;
    let synced = datastore::sync(api, &resource_id, &[], &records)?;
    Ok(Mirror {
        table,
        rows: synced.upserted.len(),
    })
}

/// The live dataset's id and the id of its DataStore resource, when the dataset has one.
///
/// CKAN addresses a resource by id only, and the id was minted when the table was
/// created; the dataset is where a later run finds it again, by the name it was given.
fn live_table(api: &impl CkanApi, dataset: &str) -> Result<(String, Option<String>), Error> {
    let Some(live) = api
        .show("package_show", dataset)
        .map_err(PublishError::from)?
    else {
        return Ok((dataset.to_owned(), None));
    };
    let package_id = live
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or(dataset)
        .to_owned();
    let resource = live
        .get("resources")
        .and_then(Value::as_array)
        .and_then(|resources| {
            resources.iter().find(|resource| {
                resource.get("url_type").and_then(Value::as_str) == Some("datastore")
                    && resource.get("name").and_then(Value::as_str) == Some(DATASTORE_RESOURCE)
            })
        })
        .and_then(|resource| resource.get("id"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    Ok((package_id, resource))
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
