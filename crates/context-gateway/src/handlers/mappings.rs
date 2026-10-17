//! The Mappings an Endpoint publishes beside its model (EP-53, EP-68): `schema/v{major}/mappings/`
//! lists them and `schema/v{major}/mappings/{file}` serves one file (API/02 §7a, "Mappings").
//!
//! The table is built from the repository the gateway applies, so every Mapping in it is a merged
//! Change; a preview's is never added. A Mapping reads source class and slot names, so a caller
//! sees one only when every name it reads is in the JSON Schema the broker's view serves that
//! caller: a Mapping over a hidden slot would name it (EP-47).

use crate::app::{admit, sha256_hex, Gateway};
use crate::handlers::schema_routes::{revalidated, revalidated_text, view_document};
use crate::middleware::tenancy;
use crate::pdp::evaluator::Subject;
use crate::resolver::Endpoint;
use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::http::Response;
use axum::response::IntoResponse;
use jc_core::kinds::MappingSpec;
use jc_core::ProblemDetails;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

/// One file of a published Mapping, held as the bytes a caller receives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MappingFile {
    /// `{mapping}.linkml-map.yaml`, `{mapping}.blobl` or `{mapping}.ir.json`.
    pub name: String,
    /// Its media type.
    pub media_type: &'static str,
    /// Its bytes.
    pub bytes: Arc<[u8]>,
}

/// One Mapping an Endpoint publishes under `schema/v{major}/mappings/`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedMapping {
    /// The manifest name.
    pub name: String,
    /// `metadata.title`, per language.
    pub title: Value,
    /// The source model and version, as the manifest names them.
    pub source: (String, String),
    /// The target model and version.
    pub target: (String, String),
    /// The major of the source model, the `v{major}` it is published under.
    pub major: u32,
    /// The specification as YAML, then the compiled Bloblang and gateway IR.
    pub files: Vec<MappingFile>,
    /// Every source class the Mapping reads, with the slots it reads of it.
    pub reads: BTreeMap<String, BTreeSet<String>>,
}

/// The published Mappings, by Endpoint slug.
pub type Mappings = BTreeMap<String, Vec<Arc<PublishedMapping>>>;

impl PublishedMapping {
    /// The Mapping `name` of `spec`, with its two compiled artifacts read beside it, or `None`
    /// when it may not be published: a `native` block (names nobody can check against a grant),
    /// a member named `secretRef` anywhere, a source major that is not a number, or a
    /// transformation whose derivations cannot be read.
    pub fn of(
        name: &str,
        title: Value,
        spec: &MappingSpec,
        bloblang: String,
        gateway_ir: String,
    ) -> Option<Self> {
        if !spec.native.is_empty() || names_secret_ref(&spec.transformation) {
            return None;
        }
        let major = spec.source.version.split('.').next()?.parse().ok()?;
        let reads = reads(&spec.transformation)?;
        let yaml = serde_norway::to_string(&spec.transformation).ok()?;
        let file = |suffix: &str, media_type, text: String| MappingFile {
            name: format!("{name}{suffix}"),
            media_type,
            bytes: Arc::from(text.into_bytes()),
        };
        Some(Self {
            name: name.to_owned(),
            title,
            source: (spec.source.name.clone(), spec.source.version.clone()),
            target: (spec.target.name.clone(), spec.target.version.clone()),
            major,
            files: vec![
                file(".linkml-map.yaml", "text/yaml", yaml),
                file(".blobl", "text/plain", bloblang),
                file(".ir.json", "application/json", gateway_ir),
            ],
            reads,
        })
    }

    /// Whether every class and slot this Mapping reads is in `visible`.
    fn readable_in(&self, visible: &BTreeMap<String, BTreeSet<String>>) -> bool {
        self.reads.iter().all(|(class, slots)| {
            visible
                .get(class)
                .is_some_and(|shown| slots.iter().all(|slot| shown.contains(slot)))
        })
    }

    /// Its entry in the mappings index: names, models and one descriptor per file.
    pub fn entry(&self) -> Value {
        let artifacts: serde_json::Map<String, Value> = self
            .files
            .iter()
            .map(|file| {
                (
                    file.name.clone(),
                    json!({
                        "type": file.media_type,
                        "bytes": file.bytes.len(),
                        "sha256": sha256_hex(&file.bytes),
                    }),
                )
            })
            .collect();
        json!({
            "name": self.name,
            "title": self.title,
            "source": { "name": self.source.0, "version": self.source.1 },
            "target": { "name": self.target.0, "version": self.target.1 },
            "artifacts": artifacts,
        })
    }
}

fn names_secret_ref(value: &Value) -> bool {
    match value {
        Value::Object(members) => members
            .iter()
            .any(|(name, member)| name == "secretRef" || names_secret_ref(member)),
        Value::Array(items) => items.iter().any(names_secret_ref),
        _ => false,
    }
}

/// The source classes and slots a LinkML-Map `TransformationSpecification` reads, from its
/// `class_derivations` (a map by target name, or a list of derivations with a `name`).
///
/// A class derivation reads `populated_from`, or the class of its own name; a slot derivation
/// reads the first segment of `populated_from`, every slot its `expr` names ([`expr_reads`]), or
/// the slot of its own name. Anything else in the specification names no source term; an `expr`
/// that cannot be read makes the whole transformation unreadable.
pub fn reads(transformation: &Value) -> Option<BTreeMap<String, BTreeSet<String>>> {
    let mut out: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (target, derivation) in named(transformation.get("class_derivations"))? {
        let class = match derivation.get("populated_from") {
            None | Some(Value::Null) => target,
            Some(source) => source.as_str()?.to_owned(),
        };
        let slots = out.entry(class).or_default();
        for (slot, slot_derivation) in named(derivation.get("slot_derivations"))? {
            let from = slot_derivation
                .get("populated_from")
                .and_then(Value::as_str);
            let expr = slot_derivation.get("expr").and_then(Value::as_str);
            match (from, expr) {
                (Some(path), _) => {
                    slots.insert(path.split('.').next().unwrap_or(path).to_owned());
                }
                (None, Some(expr)) => slots.extend(expr_reads(expr)?),
                (None, None) => {
                    slots.insert(slot);
                }
            }
        }
    }
    Some(out)
}

/// The derivations of a map or a list by name; `None` for anything else, an absent one empty.
fn named(value: Option<&Value>) -> Option<Vec<(String, &Value)>> {
    match value {
        None | Some(Value::Null) => Some(Vec::new()),
        Some(Value::Object(members)) => Some(
            members
                .iter()
                .map(|(name, derivation)| (name.clone(), derivation))
                .collect(),
        ),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| Some((item.get("name")?.as_str()?.to_owned(), item)))
            .collect(),
        Some(_) => None,
    }
}

/// Words of a LinkML-Map expression that name no slot.
const EXPR_KEYWORDS: &[&str] = &[
    "and", "or", "not", "if", "else", "in", "is", "None", "True", "False",
];
/// The expression language's functions; a name counts as one only where it is called.
const EXPR_FUNCTIONS: &[&str] = &[
    "abs", "bool", "case", "float", "int", "len", "max", "min", "round", "str",
];

/// Every slot a LinkML-Map expression reads (T-4423, EP-47): each `{name}`, and each bare name
/// that is not a keyword, a called function or an attribute after a `.`, since LinkML-Map
/// evaluates bare slot names too. String literals and numbers read nothing. Conservative: an
/// expression with a character outside that small language, an unclosed string or an unclosed
/// brace is `None`, so its Mapping is never published rather than published past the filter.
fn expr_reads(expr: &str) -> Option<Vec<String>> {
    let chars: Vec<char> = expr.chars().collect();
    let word = |c: char| c.is_alphanumeric() || c == '_';
    let mut out = Vec::new();
    let mut after_dot = false;
    let mut i = 0;
    while let Some(&c) = chars.get(i) {
        if c.is_whitespace() {
            i += 1;
        } else if c == '\'' || c == '"' {
            let mut j = i + 1;
            loop {
                match chars.get(j) {
                    None => return None,
                    Some('\\') => j += 2,
                    Some(&q) if q == c => break,
                    Some(_) => j += 1,
                }
            }
            i = j + 1;
            after_dot = false;
        } else if c == '{' {
            let close = chars[i..].iter().position(|&d| d == '}')? + i;
            let name: String = chars[i + 1..close]
                .iter()
                .collect::<String>()
                .trim()
                .to_owned();
            if name.is_empty() || !name.chars().all(word) {
                return None;
            }
            out.push(name);
            i = close + 1;
            after_dot = false;
        } else if c.is_ascii_digit() {
            while chars.get(i).is_some_and(|&d| word(d) || d == '.') {
                i += 1;
            }
            after_dot = false;
        } else if word(c) {
            let start = i;
            while chars.get(i).is_some_and(|&d| word(d)) {
                i += 1;
            }
            let name: String = chars[start..i].iter().collect();
            let called = chars[i..].iter().find(|d| !d.is_whitespace()) == Some(&'(');
            let names_no_slot = after_dot
                || EXPR_KEYWORDS.contains(&name.as_str())
                || (called && EXPR_FUNCTIONS.contains(&name.as_str()));
            if !names_no_slot {
                out.push(name);
            }
            after_dot = false;
        } else if c == '.' {
            after_dot = true;
            i += 1;
        } else if "+-*/%()<>=!,[]:".contains(c) {
            after_dot = false;
            i += 1;
        } else {
            return None;
        }
    }
    Some(out)
}

/// The classes and slots of a JSON Schema the broker's view serves (`definitions`).
fn visible_names(schema: &Value) -> BTreeMap<String, BTreeSet<String>> {
    schema
        .get("definitions")
        .and_then(Value::as_object)
        .map(|definitions| {
            definitions
                .iter()
                .map(|(class, definition)| {
                    let slots = definition
                        .get("properties")
                        .and_then(Value::as_object)
                        .map(|properties| properties.keys().cloned().collect())
                        .unwrap_or_default();
                    (class.clone(), slots)
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The Mappings of `endpoint` at `major` that `subject` may read, and whether any was left out.
/// A view that serves this caller no schema of the major hides every Mapping of it.
pub(crate) async fn visible(
    gateway: &Gateway,
    endpoint: &Endpoint,
    subject: &Subject,
    major: u32,
) -> (Vec<Arc<PublishedMapping>>, bool) {
    let published: Vec<Arc<PublishedMapping>> = gateway
        .mappings(&endpoint.slug)
        .into_iter()
        .filter(|mapping| mapping.major == major)
        .collect();
    if published.is_empty() {
        return (published, false);
    }
    let rest = format!("schema/v{major}/model.schema.json");
    let names = match view_document(gateway, endpoint, &endpoint.slug, subject, &rest).await {
        Some(Ok(bytes)) => serde_json::from_slice(&bytes)
            .map(|schema: Value| visible_names(&schema))
            .unwrap_or_default(),
        _ => BTreeMap::new(),
    };
    let total = published.len();
    let shown: Vec<_> = published
        .into_iter()
        .filter(|mapping| mapping.readable_in(&names))
        .collect();
    let redacted = shown.len() < total;
    (shown, redacted)
}

/// Every Mapping `subject` may read on `endpoint`, as the `mappings` member the record lists
/// its files from (EP-68): one entry per Mapping with its `version`.
pub(crate) async fn record_entries(
    gateway: &Gateway,
    endpoint: &Endpoint,
    subject: &Subject,
) -> Vec<Value> {
    let majors: BTreeSet<u32> = gateway
        .mappings(&endpoint.slug)
        .iter()
        .map(|mapping| mapping.major)
        .collect();
    let mut entries = Vec::new();
    for major in majors {
        for mapping in visible(gateway, endpoint, subject, major).await.0 {
            let mut entry = mapping.entry();
            entry["version"] = json!(major);
            entries.push(entry);
        }
    }
    entries
}

/// `index` with the `mappings` member [`record_entries`] lists, when there is one to add.
pub(crate) async fn with_mappings(
    gateway: &Gateway,
    endpoint: &Endpoint,
    subject: &Subject,
    mut index: Value,
) -> Value {
    let entries = record_entries(gateway, endpoint, subject).await;
    if let (false, Some(members)) = (entries.is_empty(), index.as_object_mut()) {
        members.insert("mappings".to_owned(), Value::Array(entries));
    }
    index
}

fn major_of(version: &str) -> Option<u32> {
    version.strip_prefix('v')?.parse().ok()
}

/// `GET …/schema/v{major}/mappings/`: the Mappings of the major this caller may read.
pub(crate) async fn mappings_index(
    State(gateway): State<Arc<Gateway>>,
    Path((slug, version)): Path<(String, String)>,
    mut request: Request,
) -> Response<Body> {
    tenancy::strip_client_headers(&mut request);
    let (endpoint, subject) = match admit(&gateway, &slug, None, request.headers()) {
        Ok(admitted) => admitted,
        Err(problem) => return *problem,
    };
    let Some(major) = major_of(&version) else {
        return ProblemDetails::not_found().into_response();
    };
    let (shown, redacted) = visible(&gateway, &endpoint, &subject, major).await;
    let mut index = json!({
        "endpoint": endpoint.slug,
        "version": major,
        "mappings": shown.iter().map(|mapping| mapping.entry()).collect::<Vec<_>>(),
    });
    if redacted {
        index["redacted"] = json!(true);
    }
    revalidated(&index, "application/json", request.headers())
}

/// `GET …/schema/v{major}/mappings/{file}`: one file of a Mapping this caller may read.
pub(crate) async fn mapping_file(
    State(gateway): State<Arc<Gateway>>,
    Path((slug, version, file)): Path<(String, String, String)>,
    mut request: Request,
) -> Response<Body> {
    tenancy::strip_client_headers(&mut request);
    let (endpoint, subject) = match admit(&gateway, &slug, None, request.headers()) {
        Ok(admitted) => admitted,
        Err(problem) => return *problem,
    };
    let Some(major) = major_of(&version) else {
        return ProblemDetails::not_found().into_response();
    };
    let (shown, _) = visible(&gateway, &endpoint, &subject, major).await;
    match shown
        .iter()
        .flat_map(|mapping| mapping.files.iter())
        .find(|candidate| candidate.name == file)
    {
        Some(found) => revalidated_text(found.bytes.to_vec(), found.media_type, request.headers()),
        None => ProblemDetails::not_found().into_response(),
    }
}
