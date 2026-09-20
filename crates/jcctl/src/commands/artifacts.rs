//! `jcctl artifacts rebuild`: the artifact store, re-rendered from Git (DM-44, PF-29, T-0827).
//!
//! The store is derivative: every object in it is a file the repository already declares, so a
//! rebuild after a loss is a copy of the repository into the store's own layout, with the index
//! that says what each object is. Nothing here talks to S3 — the scoped credential belongs to
//! the client that mirrors the directory into the bucket (PF-32, T-0422) — and nothing here
//! renders a missing artifact: an artifact the repository does not hold is named as missing, so
//! an operator sees what `jcctl model generate` still has to produce.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::loader::{RawManifest, Repository};

/// What one rebuild was told.
#[derive(Debug, Clone)]
pub struct Options {
    /// Where the store's objects are written; one directory per prefix.
    pub out_dir: PathBuf,
    /// Only this context space, when given (DM-44).
    pub space: Option<String>,
    /// The commit the rebuild was taken at, recorded in every index.
    pub revision: Option<String>,
}

/// What a rebuild produced.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Report {
    /// Store keys written, in order.
    pub written: Vec<String>,
    /// `kind/name: spec.artifacts.x` for every artifact the repository does not hold.
    pub missing: Vec<String>,
}

/// Why a rebuild could not be made.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The repository did not load.
    #[error("{0}")]
    Repository(String),
    /// A file could not be read or written.
    #[error("{path}: {reason}")]
    File {
        /// The file that failed.
        path: String,
        /// What the filesystem said.
        reason: String,
    },
}

/// Re-renders every artifact the repository declares into the store's layout (DM-44).
pub fn rebuild(repo_dir: &Path, options: &Options) -> Result<Report, Error> {
    let repository =
        Repository::load(repo_dir).map_err(|error| Error::Repository(error.to_string()))?;
    let mut report = Report::default();
    let revision = options.revision.as_deref().unwrap_or("unknown");

    for (id, loaded) in repository.iter() {
        let manifest = &loaded.manifest;
        let space = space_of(manifest);
        if let (Some(wanted), Some(space)) = (&options.space, space) {
            if wanted != space {
                continue;
            }
        }
        let (Some(space), Some(project)) = (space, id.namespace.as_deref()) else {
            continue;
        };
        let organization = organization_of(&repository).unwrap_or_else(|| project.to_owned());
        let source_dir = repo_dir.join(&loaded.path).parent().map(Path::to_path_buf);
        let Some(source_dir) = source_dir else {
            continue;
        };

        let (prefix, files) = match manifest.kind.as_str() {
            "DataModel" => (
                format!(
                    "schemas/{organization}/{project}/{space}/{}/v{}",
                    manifest.metadata.name,
                    major_of(manifest)
                ),
                declared(
                    manifest,
                    &["linkml"],
                    &["jsonSchema", "context", "docs", "example"],
                ),
            ),
            "Mapping" => (
                format!(
                    "mappings/{organization}/{project}/{space}/{}",
                    manifest.metadata.name
                ),
                declared(manifest, &[], &["bloblang", "gatewayIr"]),
            ),
            _ => continue,
        };

        let mut index = BTreeMap::new();
        for (field, relative) in files {
            let path = source_dir.join(&relative);
            let Ok(body) = std::fs::read(&path) else {
                report
                    .missing
                    .push(format!("{}/{field}", manifest.metadata.name));
                continue;
            };
            let name = Path::new(&relative)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or(relative.clone());
            let key = format!("{prefix}/{name}");
            write(&options.out_dir.join(&key), &body)?;
            index.insert(
                name,
                json!({
                    "field": field,
                    "bytes": body.len(),
                    "sha256": format!("{:x}", Sha256::digest(&body)),
                }),
            );
            report.written.push(key);
        }
        if index.is_empty() {
            continue;
        }
        let index = json!({
            "kind": manifest.kind,
            "name": manifest.metadata.name,
            "namespace": project,
            "space": space,
            "version": manifest.spec.get("version").cloned().unwrap_or(Value::Null),
            "sourceRevision": revision,
            "objects": index,
        });
        let key = format!("{prefix}/index.json");
        write(
            &options.out_dir.join(&key),
            format!("{index:#}\n").as_bytes(),
        )?;
        report.written.push(key);
    }
    Ok(report)
}

/// The artifacts one manifest declares, as `(field, relative path)`: the named spec members
/// first, then the members of `spec.artifacts`.
fn declared(manifest: &RawManifest, top: &[&str], artifacts: &[&str]) -> Vec<(String, String)> {
    let mut found = Vec::new();
    for field in top {
        if let Some(path) = manifest.spec.get(*field).and_then(Value::as_str) {
            found.push(((*field).to_owned(), path.to_owned()));
        }
    }
    for field in artifacts {
        if let Some(path) = manifest
            .spec
            .get("artifacts")
            .and_then(|a| a.get(*field))
            .and_then(Value::as_str)
        {
            found.push((format!("artifacts.{field}"), path.to_owned()));
        }
    }
    found
}

/// The space a manifest belongs to, for the store's prefix.
fn space_of(manifest: &RawManifest) -> Option<&str> {
    manifest.spec.get("contextSpaceRef").and_then(|value| {
        value
            .as_str()
            .or_else(|| value.get("name").and_then(Value::as_str))
    })
}

/// The organization the repository declares, whose domain names every prefix.
fn organization_of(repository: &Repository) -> Option<String> {
    repository
        .iter()
        .map(|(_, loaded)| &loaded.manifest)
        .find(|manifest| manifest.kind == "Organization")
        .map(|manifest| manifest.metadata.name.clone())
}

/// The served major of a model's version (DM-22); `0` when the manifest names none.
fn major_of(manifest: &RawManifest) -> u64 {
    manifest
        .spec
        .get("version")
        .and_then(Value::as_str)
        .and_then(|version| version.split('.').next()?.parse().ok())
        .unwrap_or(0)
}

fn write(path: &Path, body: &[u8]) -> Result<(), Error> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| Error::File {
            path: parent.display().to_string(),
            reason: error.to_string(),
        })?;
    }
    std::fs::write(path, body).map_err(|error| Error::File {
        path: path.display().to_string(),
        reason: error.to_string(),
    })
}
