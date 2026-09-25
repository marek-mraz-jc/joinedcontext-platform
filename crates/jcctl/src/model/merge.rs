//! `jcctl model merge`: the models of one space become one (DM-62, ADR-N-033).
//!
//! A space that holds several `DataModel`s is migrated to the one model DM-61 allows. The merge
//! keeps every class name, every slot and class IRI and every enum as it was, so no entity id,
//! type or attribute IRI changes and nothing is re-ingested. An IRI that a model left to its
//! `default_prefix` is written out explicitly when that prefix is not the merged model's, because
//! the same class under another default prefix would be another term. Two definitions of one
//! name that are not the same definition are a clash, and the merge refuses it naming both.

use crate::loader::Repository;
use jc_core::kinds::data_model::{DataModelLifecycle, DataModelSpec, GeneratedArtifacts};
use serde_norway::{Mapping, Value};
use std::fmt;
use std::path::{Path, PathBuf};

/// Why a merge did not happen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeError {
    /// The same name means two different things in two models.
    Clash {
        /// `class`, `slot`, `enum`, `type`, `subset` or `prefix`.
        what: &'static str,
        /// The clashing name.
        name: String,
        /// The first model that defines it.
        first: String,
        /// The second model that defines it differently.
        second: String,
    },
    /// A source that is not a LinkML schema document.
    Source {
        /// The model it belongs to.
        model: String,
        /// What is wrong with it.
        reason: String,
    },
    /// The repository does not hold what the merge needs.
    Repository(String),
}

impl fmt::Display for MergeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Clash {
                what,
                name,
                first,
                second,
            } => write!(
                f,
                "{what} `{name}` is defined differently in model `{first}` and in model \
                 `{second}`; rename one of them, or make the two definitions the same, before \
                 merging (DM-62)"
            ),
            Self::Source { model, reason } => write!(f, "model `{model}`: {reason}"),
            Self::Repository(reason) => f.write_str(reason),
        }
    }
}

impl std::error::Error for MergeError {}

/// One model's LinkML source, parsed, with the name of the model it belongs to.
#[derive(Debug, Clone)]
pub struct Source {
    /// The `DataModel`'s `metadata.name`.
    pub model: String,
    /// The LinkML document.
    pub schema: Value,
}

/// The sections whose members are merged by name.
const SECTIONS: [(&str, &str); 5] = [
    ("classes", "class"),
    ("slots", "slot"),
    ("enums", "enum"),
    ("types", "type"),
    ("subsets", "subset"),
];

/// Merges several LinkML documents into one named `name` (DM-62).
///
/// The first source is the base: its `id`, `default_prefix`, `default_range` and every other
/// top-level member stay; `prefixes` and `imports` are the union; classes, slots, enums, types
/// and subsets are the union by name. A member of another source that left its IRI to its own
/// `default_prefix` gets that IRI written out, and a slot that left its range to its own
/// `default_range` gets that range, so every term keeps the meaning it had.
pub fn merge_sources(name: &str, sources: &[Source]) -> Result<Value, MergeError> {
    let Some((base, rest)) = sources.split_first() else {
        return Err(MergeError::Repository("there is no model to merge".into()));
    };
    let mut merged = mapping_of(base)?.clone();
    let base_prefix = default_prefix(base)?;
    let base_range = text(&merged, "default_range");

    merged.insert(key("name"), Value::String(name.to_owned()));
    if base.model != name {
        if let Some(id) = text(&merged, "id") {
            let renamed = match id.rsplit_once('/') {
                Some((stem, _)) => format!("{stem}/{name}"),
                None => name.to_owned(),
            };
            merged.insert(key("id"), Value::String(renamed));
        }
    }

    let mut owners: Vec<(String, String, String)> = Vec::new();
    for (section, what) in SECTIONS {
        for (member, _) in members(&merged, section) {
            owners.push((what.to_owned(), member, base.model.clone()));
        }
    }

    for source in rest {
        let schema = explicit(source, &base_prefix, base_range.as_deref())?;
        merge_prefixes(&mut merged, &schema, &base.model, &source.model)?;
        merge_imports(&mut merged, &schema);
        for (section, what) in SECTIONS {
            for (member, definition) in members(&schema, section) {
                let existing = merged
                    .get(key(section))
                    .and_then(Value::as_mapping)
                    .and_then(|members| members.get(key(&member)));
                match existing {
                    Some(same)
                        if section != "slots" && *same == definition
                            || section == "slots"
                                && ranged(same, base_range.as_deref())
                                    == ranged(&definition, base_range.as_deref()) =>
                    {
                        continue
                    }
                    Some(_) => {
                        let first = owners
                            .iter()
                            .find(|(kind, owned, _)| kind == what && *owned == member)
                            .map(|(_, _, model)| model.clone())
                            .unwrap_or_else(|| base.model.clone());
                        return Err(MergeError::Clash {
                            what,
                            name: member,
                            first,
                            second: source.model.clone(),
                        });
                    }
                    None => {
                        section_mut(&mut merged, section)?.insert(key(&member), definition);
                        owners.push((what.to_owned(), member, source.model.clone()));
                    }
                }
            }
        }
    }
    Ok(Value::Mapping(merged))
}

/// One source with every IRI and range it left to its own defaults written out, when those
/// defaults are not the merged model's.
fn explicit(
    source: &Source,
    base_prefix: &Option<String>,
    base_range: Option<&str>,
) -> Result<Mapping, MergeError> {
    let mut schema = mapping_of(source)?.clone();
    let own_prefix = default_prefix(source)?;
    let own_range = text(&schema, "default_range");
    let prefix = own_prefix.filter(|own| Some(own) != base_prefix.as_ref());
    let range = own_range.filter(|own| Some(own.as_str()) != base_range);

    let uri_of = |member: &str| prefix.as_ref().map(|p| format!("{p}:{member}"));
    for (section, field) in [
        ("classes", "class_uri"),
        ("slots", "slot_uri"),
        ("enums", "enum_uri"),
        ("types", "uri"),
    ] {
        let Some(members) = schema.get_mut(key(section)).and_then(Value::as_mapping_mut) else {
            continue;
        };
        for (member, definition) in members.iter_mut() {
            let (Some(member), Some(definition)) = (member.as_str(), definition.as_mapping_mut())
            else {
                continue;
            };
            // A type's IRI is its `uri`, which LinkML requires; an unset one is left for the
            // compiler to refuse rather than guessed here.
            if section != "types" && !definition.contains_key(key(field)) {
                if let Some(uri) = uri_of(member) {
                    definition.insert(key(field), Value::String(uri));
                }
            }
            if section == "slots" {
                fill_range(definition, range.as_deref());
            }
            if section == "classes" {
                if let Some(attributes) = definition
                    .get_mut(key("attributes"))
                    .and_then(Value::as_mapping_mut)
                {
                    for (attribute, slot) in attributes.iter_mut() {
                        let (Some(attribute), Some(slot)) =
                            (attribute.as_str(), slot.as_mapping_mut())
                        else {
                            continue;
                        };
                        if !slot.contains_key(key("slot_uri")) {
                            if let Some(uri) = uri_of(attribute) {
                                slot.insert(key("slot_uri"), Value::String(uri));
                            }
                        }
                        fill_range(slot, range.as_deref());
                    }
                }
            }
        }
    }
    Ok(schema)
}

/// A slot definition with the range it gets by default written out, so a slot that relies on
/// `default_range: string` and one that says `range: string` compare as the same slot.
fn ranged(slot: &Value, default_range: Option<&str>) -> Value {
    let mut slot = slot.clone();
    if let Some(definition) = slot.as_mapping_mut() {
        fill_range(definition, default_range);
    }
    slot
}

fn fill_range(slot: &mut Mapping, range: Option<&str>) {
    if let Some(range) = range {
        if !slot.contains_key(key("range")) {
            slot.insert(key("range"), Value::String(range.to_owned()));
        }
    }
}

fn merge_prefixes(
    merged: &mut Mapping,
    schema: &Mapping,
    base: &str,
    model: &str,
) -> Result<(), MergeError> {
    for (prefix, iri) in members(schema, "prefixes") {
        let existing = merged
            .get(key("prefixes"))
            .and_then(Value::as_mapping)
            .and_then(|prefixes| prefixes.get(key(&prefix)));
        match existing {
            Some(same) if *same == iri => {}
            Some(_) => {
                return Err(MergeError::Clash {
                    what: "prefix",
                    name: prefix,
                    first: base.to_owned(),
                    second: model.to_owned(),
                })
            }
            None => {
                section_mut(merged, "prefixes")?.insert(key(&prefix), iri);
            }
        }
    }
    Ok(())
}

fn merge_imports(merged: &mut Mapping, schema: &Mapping) {
    let Some(imports) = schema.get(key("imports")).and_then(Value::as_sequence) else {
        return;
    };
    let entry = merged
        .entry(key("imports"))
        .or_insert_with(|| Value::Sequence(Vec::new()));
    if let Some(list) = entry.as_sequence_mut() {
        for import in imports {
            if !list.contains(import) {
                list.push(import.clone());
            }
        }
    }
}

/// The source's `default_prefix`, which must be one of its `prefixes`: an IRI written out from
/// a prefix nobody declares would not resolve.
fn default_prefix(source: &Source) -> Result<Option<String>, MergeError> {
    let schema = mapping_of(source)?;
    let Some(prefix) = text(schema, "default_prefix") else {
        return Ok(None);
    };
    let declared = schema
        .get(key("prefixes"))
        .and_then(Value::as_mapping)
        .is_some_and(|prefixes| prefixes.contains_key(key(&prefix)));
    if declared {
        Ok(Some(prefix))
    } else {
        Err(MergeError::Source {
            model: source.model.clone(),
            reason: format!("default_prefix `{prefix}` is not one of its prefixes"),
        })
    }
}

fn mapping_of(source: &Source) -> Result<&Mapping, MergeError> {
    source
        .schema
        .as_mapping()
        .ok_or_else(|| MergeError::Source {
            model: source.model.clone(),
            reason: "the LinkML source is not a YAML mapping".into(),
        })
}

fn members(schema: &Mapping, section: &str) -> Vec<(String, Value)> {
    schema
        .get(key(section))
        .and_then(Value::as_mapping)
        .into_iter()
        .flatten()
        .filter_map(|(name, value)| Some((name.as_str()?.to_owned(), value.clone())))
        .collect()
}

fn section_mut<'a>(schema: &'a mut Mapping, section: &str) -> Result<&'a mut Mapping, MergeError> {
    schema
        .entry(key(section))
        .or_insert_with(|| Value::Mapping(Mapping::new()))
        .as_mapping_mut()
        .ok_or_else(|| {
            MergeError::Repository(format!("`{section}` of the base model is not a mapping"))
        })
}

fn text(schema: &Mapping, field: &str) -> Option<String> {
    schema
        .get(key(field))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn key(name: &str) -> Value {
    Value::String(name.to_owned())
}

/// What `jcctl model merge` wrote and removed, repository-relative.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MergeReport {
    /// The merged model's name.
    pub model: String,
    /// The models that went into it, in merge order.
    pub merged: Vec<String>,
    /// Files written.
    pub written: Vec<PathBuf>,
    /// Files removed: the other models' manifests, sources and generated artifacts.
    pub removed: Vec<PathBuf>,
}

/// Merges every model of `space` in `project` into one model called `name` (DM-62).
///
/// The model called `name`, when the space holds one, is the base; otherwise the first by name
/// is. The merged manifest keeps the base's folder, lifecycle and metadata, carries the union of
/// the classes, and names its artifacts `{name}.v{major}.*`, for `jcctl model generate` to
/// render: the old artifacts describe the old models. The other models' manifests, sources and
/// artifacts are removed. A space with one model is left as it is.
pub fn merge(
    repo_dir: &Path,
    project: &str,
    space: &str,
    name: Option<&str>,
) -> Result<MergeReport, MergeError> {
    let repo = Repository::load(repo_dir).map_err(|err| MergeError::Repository(err.to_string()))?;
    let mut held: Vec<(String, PathBuf, DataModelSpec, serde_json::Value)> = repo
        .iter()
        .filter(|(id, _)| id.kind == "DataModel" && id.namespace.as_deref() == Some(project))
        .filter_map(|(id, resource)| {
            let spec: DataModelSpec =
                serde_json::from_value(resource.manifest.spec.clone()).ok()?;
            (spec.context_space_ref == space && spec.lifecycle != DataModelLifecycle::Mirrored)
                .then(|| {
                    let metadata = serde_json::to_value(&resource.manifest.metadata)
                        .unwrap_or(serde_json::Value::Null);
                    (id.name.clone(), resource.path.clone(), spec, metadata)
                })
        })
        .collect();
    if held.is_empty() {
        return Err(MergeError::Repository(format!(
            "project `{project}` holds no data model of space `{space}`"
        )));
    }
    held.sort_by(|a, b| a.0.cmp(&b.0));
    let name = name.unwrap_or(space).to_owned();
    jc_core::names::validate_dns1123_label(&name)
        .map_err(|err| MergeError::Repository(err.to_string()))?;
    if let Some(at) = held.iter().position(|(model, ..)| *model == name) {
        let base = held.remove(at);
        held.insert(0, base);
    }
    if held.len() == 1 && held[0].0 == name {
        return Ok(MergeReport {
            model: name,
            merged: vec![held[0].0.clone()],
            ..MergeReport::default()
        });
    }

    let mut sources = Vec::new();
    for (model, path, spec, _) in &held {
        spec.validate().map_err(|err| MergeError::Source {
            model: model.clone(),
            reason: err.to_string(),
        })?;
        let file = folder(path).join(&spec.linkml);
        let text =
            std::fs::read_to_string(repo_dir.join(&file)).map_err(|err| MergeError::Source {
                model: model.clone(),
                reason: format!("{}: {err}", file.display()),
            })?;
        let schema = serde_norway::from_str(&text).map_err(|err| MergeError::Source {
            model: model.clone(),
            reason: format!("{}: {err}", file.display()),
        })?;
        sources.push(Source {
            model: model.clone(),
            schema,
        });
    }
    let merged = merge_sources(&name, &sources)?;

    let (_, base_path, base_spec, base_metadata) = &held[0];
    let dir = folder(base_path);
    let major = base_spec.version.major();
    let mut spec = base_spec.clone();
    spec.linkml = format!("./{name}.linkml.yaml");
    for (_, _, other, _) in &held[1..] {
        for class in &other.classes {
            if !spec.classes.contains(class) {
                spec.classes.push(class.clone());
            }
        }
        for consumer in &other.consumers {
            if !spec.consumers.contains(consumer) {
                spec.consumers.push(consumer.clone());
            }
        }
        if other.open_world != spec.open_world {
            return Err(MergeError::Repository(format!(
                "model `{}` is {} and model `{}` is {}; a merged model is one or the other, so \
                 make them agree first (DM-28)",
                held[0].0,
                world(spec.open_world),
                held.iter()
                    .find(|(_, _, s, _)| s.open_world == other.open_world)
                    .map(|(m, ..)| m.as_str())
                    .unwrap_or_default(),
                world(other.open_world),
            )));
        }
    }
    spec.artifacts = GeneratedArtifacts {
        json_schema: Some(format!("./{name}.v{major}.schema.json")),
        context: Some(format!("./{name}.v{major}.context.jsonld")),
        docs: Some(format!("./{name}.v{major}.md")),
        example: Some(format!("./{name}.v{major}.example.jsonld")),
    };

    let mut metadata = base_metadata.clone();
    metadata["name"] = serde_json::Value::String(name.clone());
    let manifest = serde_json::json!({
        "apiVersion": "joinedcontext.com/v1alpha1",
        "kind": "DataModel",
        "metadata": metadata,
        "spec": spec,
    });
    let manifest_path = dir.join(format!("{name}.yaml"));
    let source_path = dir.join(format!("{name}.linkml.yaml"));
    let written = vec![manifest_path.clone(), source_path.clone()];
    // The merged model's own artifact paths are rewritten by `jcctl model generate`.
    let regenerated: Vec<PathBuf> = [
        &spec.artifacts.json_schema,
        &spec.artifacts.context,
        &spec.artifacts.docs,
        &spec.artifacts.example,
    ]
    .into_iter()
    .flatten()
    .map(|artifact| tidy(&dir.join(artifact)))
    .collect();

    let mut removed = Vec::new();
    for (_, path, other, _) in &held {
        let own = folder(path);
        let mut files = vec![path.clone(), own.join(&other.linkml)];
        files.extend(
            [
                &other.artifacts.json_schema,
                &other.artifacts.context,
                &other.artifacts.docs,
                &other.artifacts.example,
            ]
            .into_iter()
            .flatten()
            .map(|artifact| own.join(artifact)),
        );
        for file in files {
            let file = tidy(&file);
            if !written.contains(&file) && !regenerated.contains(&file) && !removed.contains(&file)
            {
                removed.push(file);
            }
        }
    }

    write(repo_dir, &source_path, &yaml(&merged)?)?;
    write(
        repo_dir,
        &manifest_path,
        &yaml(&serde_norway::to_value(&manifest).map_err(|err| {
            MergeError::Repository(format!("the merged manifest does not serialize: {err}"))
        })?)?,
    )?;
    for file in &removed {
        match std::fs::remove_file(repo_dir.join(file)) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(MergeError::Repository(format!("{}: {err}", file.display()))),
        }
    }

    Ok(MergeReport {
        model: name,
        merged: held.into_iter().map(|(model, ..)| model).collect(),
        written,
        removed,
    })
}

fn world(open: bool) -> &'static str {
    if open {
        "open-world"
    } else {
        "closed"
    }
}

fn folder(path: &Path) -> PathBuf {
    path.parent().map(Path::to_path_buf).unwrap_or_default()
}

/// Drops the `.` segments a relative artifact path carries, so two spellings of one file are
/// one entry.
fn tidy(path: &Path) -> PathBuf {
    path.components()
        .filter(|part| !matches!(part, std::path::Component::CurDir))
        .collect()
}

fn yaml(value: &Value) -> Result<String, MergeError> {
    serde_norway::to_string(value).map_err(|err| {
        MergeError::Repository(format!("the merged model does not serialize: {err}"))
    })
}

fn write(repo_dir: &Path, path: &Path, text: &str) -> Result<(), MergeError> {
    std::fs::write(repo_dir.join(path), text)
        .map_err(|err| MergeError::Repository(format!("{}: {err}", path.display())))
}
