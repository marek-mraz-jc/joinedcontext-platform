//! Seed entities: the one live state the repository describes that nothing reloads by itself
//! (CC-72, T-0421).
//!
//! Every configuration kind is read straight from Git by the component that serves it — the
//! gateway re-reads its tables every second, the runners mount their streams — so `apply` has
//! exactly one live job left: putting the entities a space is seeded with into an empty
//! broker, through the gateway, under the reconciler's own ServiceAccount (CC-04).
//!
//! Entities live beside the space that holds them, as NGSI-LD normalized JSON:
//! `projects/{project}/spaces/{space}/entities/seed/*.json`, one entity per file or an array
//! of them. They are not manifests: no `apiVersion`, no `kind`, no `metadata`, because what
//! goes into the broker is what the broker itself answers with.

use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The folder of one space that holds its seed entities.
const SEED_DIR: &str = "entities/seed";

/// What the broker adds to an entity and no repository declares, so a comparison that counted
/// them would report every seeded entity as changed forever.
const BROKER_OWNED: [&str; 4] = ["createdAt", "modifiedAt", "deletedAt", "instanceId"];

/// One seed entity as the repository declares it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeedEntity {
    /// Project whose folder holds it.
    pub project: String,
    /// Context Space it is seeded into; the `{space}` of its URN and of the gateway path.
    pub space: String,
    /// The entity's `id`, an NGSI-LD URN.
    pub id: String,
    /// The entity as written, normalized NGSI-LD.
    pub body: Value,
    /// The file it was read from, so a message can name it.
    pub source: PathBuf,
}

/// Why the seed entities of a repository could not be read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SeedError {
    /// A file or directory could not be read.
    #[error("{path}: {message}")]
    Unreadable {
        /// The path that could not be read.
        path: String,
        /// What the filesystem said.
        message: String,
    },
    /// A file is not the JSON this folder holds.
    #[error("{path}: {message}")]
    Malformed {
        /// The file.
        path: String,
        /// What is wrong with it, in one clause.
        message: String,
    },
    /// Two files seed the same id into the same space, so which one wins would be the order
    /// the filesystem happened to answer in.
    #[error("{id} is seeded into {space} by both {first} and {second}")]
    Duplicate {
        /// The entity id declared twice.
        id: String,
        /// The space both files seed.
        space: String,
        /// The first file, in path order.
        first: String,
        /// The second.
        second: String,
    },
}

/// Every seed entity of a repository checkout, in path order.
///
/// A repository with no `projects/` and a space with no seed folder both read as no entities:
/// seeding is optional, and a fresh installation has none.
pub fn seed_entities(repo_dir: &Path) -> Result<Vec<SeedEntity>, SeedError> {
    let mut found: Vec<SeedEntity> = Vec::new();
    let mut seen: BTreeMap<(String, String), PathBuf> = BTreeMap::new();

    for (project, space, dir) in seed_dirs(repo_dir)? {
        for file in json_files(&dir)? {
            for body in entities_in(&file)? {
                let id = entity_id(&body, &file)?;
                let key = (space.clone(), id.clone());
                if let Some(first) = seen.get(&key) {
                    return Err(SeedError::Duplicate {
                        id,
                        space,
                        first: first.display().to_string(),
                        second: file.display().to_string(),
                    });
                }
                seen.insert(key, file.clone());
                found.push(SeedEntity {
                    project: project.clone(),
                    space: space.clone(),
                    id,
                    body,
                    source: file.clone(),
                });
            }
        }
    }
    Ok(found)
}

/// What one declared entity needs from the broker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// The broker does not hold it.
    Create,
    /// The broker holds it, and at least one declared attribute differs.
    Update,
    /// Every declared attribute is already there, with the value the repository declares.
    Unchanged,
}

/// What `apply` would do to one declared entity, given what the broker answered for its id.
///
/// Only the declared attributes are compared. A space also holds what its pipelines and its
/// devices write — an observation, a measurement — and telemetry beside a seeded entity is
/// not drift (CC-07, CC-69); neither are the timestamps the broker itself maintains.
pub fn action(declared: &Value, live: Option<&Value>) -> Action {
    let Some(live) = live else {
        return Action::Create;
    };
    let (Some(declared), Some(live)) = (declared.as_object(), live.as_object()) else {
        return Action::Update;
    };
    for (name, value) in declared {
        if BROKER_OWNED.contains(&name.as_str()) {
            continue;
        }
        match live.get(name) {
            Some(held) if same(value, held) => {}
            _ => return Action::Update,
        }
    }
    Action::Unchanged
}

/// Whether the broker holds what the repository declares, ignoring what the broker adds of
/// its own on every attribute.
fn same(declared: &Value, live: &Value) -> bool {
    match (declared, live) {
        (Value::Object(declared), Value::Object(live)) => declared
            .iter()
            .filter(|(name, _)| !BROKER_OWNED.contains(&name.as_str()))
            .all(|(name, value)| live.get(name).is_some_and(|held| same(value, held))),
        (Value::Array(declared), Value::Array(live)) => {
            declared.len() == live.len()
                && declared
                    .iter()
                    .zip(live)
                    .all(|(one, other)| same(one, other))
        }
        _ => declared == live,
    }
}

/// `(project, space, path)` of every seed folder in the checkout, in path order.
fn seed_dirs(repo_dir: &Path) -> Result<Vec<(String, String, PathBuf)>, SeedError> {
    let projects = repo_dir.join("projects");
    let mut dirs = Vec::new();
    for project in subdirectories(&projects)? {
        let name = file_name(&project);
        for space in subdirectories(&project.join("spaces"))? {
            let seed = space.join(SEED_DIR);
            if seed.is_dir() {
                dirs.push((name.clone(), file_name(&space), seed));
            }
        }
    }
    Ok(dirs)
}

/// The directories directly inside `parent`, sorted. A missing parent is none of them.
fn subdirectories(parent: &Path) -> Result<Vec<PathBuf>, SeedError> {
    if !parent.is_dir() {
        return Ok(Vec::new());
    }
    let mut found: Vec<PathBuf> = read_dir(parent)?
        .into_iter()
        .filter(|path| path.is_dir())
        .collect();
    found.sort();
    Ok(found)
}

/// The `.json` files directly inside a seed folder, sorted.
fn json_files(dir: &Path) -> Result<Vec<PathBuf>, SeedError> {
    let mut found: Vec<PathBuf> = read_dir(dir)?
        .into_iter()
        .filter(|path| {
            path.is_file()
                && path
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("json"))
        })
        .collect();
    found.sort();
    Ok(found)
}

fn read_dir(path: &Path) -> Result<Vec<PathBuf>, SeedError> {
    let entries = std::fs::read_dir(path).map_err(|e| SeedError::Unreadable {
        path: path.display().to_string(),
        message: e.to_string(),
    })?;
    let mut found = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| SeedError::Unreadable {
            path: path.display().to_string(),
            message: e.to_string(),
        })?;
        found.push(entry.path());
    }
    Ok(found)
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
}

/// The entities one file holds: one object, or an array of them.
fn entities_in(file: &Path) -> Result<Vec<Value>, SeedError> {
    let text = std::fs::read_to_string(file).map_err(|e| SeedError::Unreadable {
        path: file.display().to_string(),
        message: e.to_string(),
    })?;
    let document: Value = serde_json::from_str(&text).map_err(|e| SeedError::Malformed {
        path: file.display().to_string(),
        message: format!("is not JSON: {e}"),
    })?;
    match document {
        Value::Object(map) => Ok(vec![Value::Object(map)]),
        Value::Array(items) => Ok(items),
        _ => Err(SeedError::Malformed {
            path: file.display().to_string(),
            message: "holds neither an entity nor an array of entities".to_owned(),
        }),
    }
}

/// The `id` of one entity, refusing anything that is not an NGSI-LD entity with a type.
fn entity_id(body: &Value, file: &Path) -> Result<String, SeedError> {
    let malformed = |message: &str| SeedError::Malformed {
        path: file.display().to_string(),
        message: message.to_owned(),
    };
    let object: &Map<String, Value> = body
        .as_object()
        .ok_or_else(|| malformed("is not an entity"))?;
    if object.contains_key("apiVersion") || object.contains_key("kind") {
        return Err(malformed(
            "is a manifest; this folder holds NGSI-LD entities, which carry no apiVersion or kind",
        ));
    }
    let id = object
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| malformed("declares no id"))?;
    if id.is_empty() {
        return Err(malformed("declares an empty id"));
    }
    if !object.get("type").is_some_and(Value::is_string) {
        return Err(malformed(&format!("{id} declares no type")));
    }
    Ok(id.to_owned())
}
