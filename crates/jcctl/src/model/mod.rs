//! `jcctl model` — the command line over Model Tools (T-0406, DM-02, DM-19, DM-32).
//!
//! LinkML has no Rust implementation, so one Python service renders every artifact
//! (Architecture/11 §6.5). DM-32 asks for the same capability on the command line as in the
//! editor, and for one reason: CI must be able to regenerate what a merge request committed
//! and fail on any difference (DM-02). A second generator would make that diff meaningless,
//! so the repository pins the version it generates with and this module refuses to run
//! against anything else (DM-19).

pub mod http;

use crate::loader::{LoadedResource, Repository};
use jc_core::kinds::data_model::{DataModelSpec, GeneratedArtifacts};
use serde::Deserialize;
use serde_json::{Map, Value};
use std::fmt;
use std::path::{Path, PathBuf};

/// The file at the repository root that pins the Model Tools version (DM-19).
pub const SETTINGS_FILE: &str = "platform-settings.yaml";

/// Where Model Tools is, when `--url` names no address: `JC_MODEL_TOOLS_URL`. Without either,
/// a command that needs the generators says so rather than guessing a service name (DM-18).
pub const URL_ENV: &str = "JC_MODEL_TOOLS_URL";

/// The DM-02 committed set: the answer field, and the manifest path that field is written to.
///
/// Model Tools renders more than this (SHACL, OWL) and DM-44 uploads the full set to the
/// artifact store; only these four live in Git, so only these four are written and compared.
fn committed_set(artifacts: &GeneratedArtifacts) -> [(&'static str, Option<&String>); 4] {
    [
        ("jsonSchema", artifacts.json_schema.as_ref()),
        ("context", artifacts.context.as_ref()),
        ("docs", artifacts.docs.as_ref()),
        ("example", artifacts.example.as_ref()),
    ]
}

/// What a run does to the working tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Write every declared artifact whose rendering differs from what is committed.
    Generate,
    /// Compare only; a difference is reported and the tree is left alone (DM-02).
    Diff,
    /// Compile every model and report what does not compile; no artifact is read or written.
    Validate,
}

/// Anything that stops a run before it can report per model.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Model Tools could not be reached or did not answer usably.
    #[error("{0}")]
    Http(#[from] http::HttpError),
    /// Model Tools answered, with a status that is not a result.
    #[error("Model Tools answered {status}: {message}")]
    Service {
        /// The HTTP status.
        status: u16,
        /// What the service said, or the body when it said nothing structured.
        message: String,
    },
    /// The repository could not be read.
    #[error("{0}")]
    Repository(String),
    /// The pin is missing or unreadable.
    #[error("{0}")]
    Settings(String),
    /// The service is not the version the repository generates with (DM-19).
    #[error(
        "the repository pins Model Tools {pinned} and {url} runs {running}; regenerating with \
         another generator rewrites committed artifacts for a reason no diff can show. Start the \
         pinned image, or change modelTools.generatorVersion in platform-settings.yaml in the \
         commit that carries the artifacts it renders differently (DM-19)"
    )]
    Pin {
        /// What `platform-settings.yaml` names.
        pinned: String,
        /// What `GET /healthz` reports.
        running: String,
        /// The service that was asked.
        url: String,
    },
    /// A file under the repository could not be read or written.
    #[error("{path}: {source}")]
    Io {
        /// Repository-relative path.
        path: PathBuf,
        /// The underlying failure.
        #[source]
        source: std::io::Error,
    },
}

/// One model's outcome, in the shape API/03 §4 documents.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ModelReport {
    /// `DataModel/{namespace}/{name}`.
    pub id: String,
    /// Repository-relative path of the LinkML source.
    pub linkml: String,
    /// Artifacts written by this run.
    pub written: Vec<String>,
    /// Committed artifacts that no longer match a fresh rendering.
    pub stale: Vec<String>,
    /// Artifacts `spec.artifacts` declares that the answer does not carry.
    pub missing: Vec<String>,
    /// What the model does not compile to, as Model Tools reported it.
    pub errors: Vec<String>,
}

impl ModelReport {
    fn to_json(&self) -> Value {
        serde_json::json!({
            "id": self.id,
            "linkml": self.linkml,
            "written": self.written,
            "stale": self.stale,
            "missing": self.missing,
            "errors": self.errors,
        })
    }
}

/// The whole run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// The version that rendered everything in this run (DM-19, DM-43).
    pub generator_version: String,
    /// One entry per DataModel in the repository, in repository order.
    pub models: Vec<ModelReport>,
}

impl Report {
    /// Committed artifacts that no longer match, across every model.
    pub fn stale(&self) -> usize {
        self.models.iter().map(|m| m.stale.len()).sum()
    }

    /// Models that did not compile, or declared an artifact the answer does not carry.
    pub fn failed(&self) -> usize {
        self.models
            .iter()
            .filter(|m| !m.errors.is_empty() || !m.missing.is_empty())
            .count()
    }

    /// The JSON one line of stdout carries (API/03 §4).
    pub fn to_json(&self) -> Value {
        serde_json::json!({
            "generatorVersion": self.generator_version,
            "models": self.models.iter().map(ModelReport::to_json).collect::<Vec<_>>(),
            "stale": self.stale(),
            "failed": self.failed(),
        })
    }
}

/// One answer of `POST /generate` or `POST /import-sdm`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Answer {
    /// What rendered it.
    pub generator_version: String,
    /// Every rendered field, keyed as the service names it (`jsonSchema`, `context`, …).
    pub artifacts: Map<String, Value>,
    /// What the source does not compile to; an empty list means it compiled.
    pub errors: Vec<String>,
}

impl Answer {
    fn parse(body: &str) -> Result<Self, Error> {
        let mut object: Map<String, Value> = match serde_json::from_str(body) {
            Ok(Value::Object(object)) => object,
            _ => {
                return Err(http::HttpError::Protocol(format!(
                    "a body that is not a JSON object: {}",
                    truncate(body)
                ))
                .into())
            }
        };
        let generator_version = match object.remove("generatorVersion") {
            Some(Value::String(version)) => version,
            _ => {
                return Err(http::HttpError::Protocol(
                    "an answer with no generatorVersion, so nothing can say what rendered it \
                     (DM-19)"
                        .into(),
                )
                .into())
            }
        };
        let errors = match object.remove("errors") {
            Some(Value::Array(items)) => items.iter().map(render_message).collect(),
            _ => Vec::new(),
        };
        Ok(Self {
            generator_version,
            artifacts: object,
            errors,
        })
    }
}

/// Model Tools, reached over plaintext inside the cluster.
#[derive(Debug, Clone)]
pub struct ModelTools {
    url: String,
}

impl ModelTools {
    /// A client for the service at `url`.
    ///
    /// The URL comes from a flag or [`URL_ENV`] and never from a manifest: a repository that
    /// could name the generator could name one that renders what it likes.
    pub fn new(url: impl Into<String>) -> Self {
        Self { url: url.into() }
    }

    /// The URL this client talks to.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// `GET /healthz`: the version running behind this URL (DM-19).
    pub fn generator_version(&self) -> Result<String, Error> {
        let body = self.call("/healthz", None)?;
        match serde_json::from_str::<Value>(&body) {
            Ok(Value::Object(object)) => match object.get("generatorVersion") {
                Some(Value::String(version)) => Ok(version.clone()),
                _ => Err(http::HttpError::Protocol(
                    "a /healthz answer with no generatorVersion".into(),
                )
                .into()),
            },
            _ => Err(http::HttpError::Protocol(format!(
                "a /healthz answer that is not a JSON object: {}",
                truncate(&body)
            ))
            .into()),
        }
    }

    /// `POST /generate`: what one LinkML document compiles to.
    pub fn generate(&self, source: &str) -> Result<Answer, Error> {
        let body = serde_json::json!({ "source": source }).to_string();
        Answer::parse(&self.call("/generate", Some(&body))?)
    }

    /// `POST /import-sdm`: one catalogue model, as the LinkML it becomes (DM-07…DM-11).
    pub fn import_sdm(&self, model: &str) -> Result<Answer, Error> {
        let body = serde_json::json!({ "model": model }).to_string();
        Answer::parse(&self.call("/import-sdm", Some(&body))?)
    }

    /// `POST /infer-schema`: a draft model from one sample file (DM-54, DM-55).
    ///
    /// The bytes travel base64 in JSON; Model Tools parses them in memory and writes nothing.
    /// The answer is the draft as it is, `linkml`, `operations` and the rest of API/01 §11.
    pub fn infer(&self, name: &str, content: &[u8]) -> Result<Value, Error> {
        use base64::Engine as _;
        let body = serde_json::json!({
            "name": name,
            "content": base64::engine::general_purpose::STANDARD.encode(content),
        })
        .to_string();
        let raw = self.call("/infer-schema", Some(&body))?;
        serde_json::from_str::<Value>(&raw)
            .ok()
            .filter(Value::is_object)
            .ok_or_else(|| {
                http::HttpError::Protocol(format!(
                    "an infer-schema answer that is not a JSON object: {}",
                    truncate(&raw)
                ))
                .into()
            })
    }

    fn call(&self, path: &str, body: Option<&str>) -> Result<String, Error> {
        let response = http::request(&self.url, path, body)?;
        if response.status == 200 {
            return Ok(response.body);
        }
        // A model that does not compile comes back as a 200 with `errors`, so a status here is
        // the request being wrong or the service being unwell, and belongs to the operator.
        Err(Error::Service {
            status: response.status,
            message: match serde_json::from_str::<Value>(&response.body) {
                Ok(Value::Object(object)) => match object.get("errors") {
                    Some(Value::Array(items)) => items
                        .iter()
                        .map(render_message)
                        .collect::<Vec<_>>()
                        .join("; "),
                    _ => truncate(&response.body),
                },
                _ => truncate(&response.body),
            },
        })
    }
}

/// The version `platform-settings.yaml` pins, or why it cannot be read (DM-19).
pub fn pinned_generator(repo_dir: &Path) -> Result<String, Error> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Settings {
        model_tools: Option<Pin>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Pin {
        generator_version: String,
    }

    // Every other key belongs to another subsystem, so unknown fields are ignored here on
    // purpose; `jcctl validate` is what checks the file as a whole.
    let path = repo_dir.join(SETTINGS_FILE);
    let text = std::fs::read_to_string(&path).map_err(|source| {
        Error::Settings(format!(
        "{}: {source}. Generation needs the Model Tools version this repository generates with \
         (DM-19)",
        path.display()
    ))
    })?;
    let settings: Settings = serde_norway::from_str(&text)
        .map_err(|err| Error::Settings(format!("{}: {err}", path.display())))?;
    settings
        .model_tools
        .map(|pin| pin.generator_version)
        .ok_or_else(|| {
            Error::Settings(format!(
                "{} names no modelTools.generatorVersion, so nothing says which generator this \
                 repository's committed artifacts came from (DM-19)",
                path.display()
            ))
        })
}

/// Runs one verb over every DataModel in the repository.
///
/// `Generate` and `Diff` check the pin first: both read committed artifacts and one of them
/// rewrites them, and doing either with the wrong generator is the failure DM-19 names.
/// `Validate` reads no artifact and writes none, so it runs against whatever is reachable.
pub fn run(repo_dir: &Path, tools: &ModelTools, mode: Mode) -> Result<Report, Error> {
    let repo = Repository::load(repo_dir).map_err(|err| Error::Repository(err.to_string()))?;
    let running = tools.generator_version()?;

    if mode != Mode::Validate {
        let pinned = pinned_generator(repo_dir)?;
        if pinned != running {
            return Err(Error::Pin {
                pinned,
                running,
                url: tools.url().to_owned(),
            });
        }
    }

    let mut models = Vec::new();
    for (id, resource) in repo.iter() {
        if resource.manifest.kind != "DataModel" {
            continue;
        }
        models.push(one(repo.root(), &id.to_string(), resource, tools, mode)?);
    }

    Ok(Report {
        generator_version: running,
        models,
    })
}

/// Renders one DataModel and writes or compares what `spec.artifacts` declares.
fn one(
    root: &Path,
    id: &str,
    resource: &LoadedResource,
    tools: &ModelTools,
    mode: Mode,
) -> Result<ModelReport, Error> {
    let mut report = ModelReport {
        id: id.to_owned(),
        ..ModelReport::default()
    };

    let spec: DataModelSpec = match serde_json::from_value(resource.manifest.spec.clone()) {
        Ok(spec) => spec,
        Err(err) => {
            report
                .errors
                .push(format!("{}: {err}", resource.path.display()));
            return Ok(report);
        }
    };
    // The same validation `jcctl validate` runs, for the reason this command needs it: it is
    // what keeps `spec.linkml` and every artifact path inside the model's own directory.
    if let Err(err) = spec.validate() {
        report
            .errors
            .push(format!("{}: {err}", resource.path.display()));
        return Ok(report);
    }

    let dir = resource.path.parent().unwrap_or(Path::new(""));
    let linkml = dir.join(&spec.linkml);
    report.linkml = display(&linkml);
    let source = read(root, &linkml)?;

    let answer = tools.generate(&source)?;
    report.errors = answer.errors;
    if mode == Mode::Validate || !report.errors.is_empty() {
        // A model that does not compile has no artifacts to write, and the half-set a failing
        // generator leaves behind would be committed as if it were whole.
        return Ok(report);
    }

    for (field, declared) in committed_set(&spec.artifacts) {
        let Some(relative) = declared else {
            continue;
        };
        let target = dir.join(relative);
        let Some(value) = answer.artifacts.get(field) else {
            report.missing.push(display(&target));
            continue;
        };
        let rendered = render(value);
        let committed = std::fs::read_to_string(root.join(&target)).ok();
        if committed.as_deref() == Some(rendered.as_str()) {
            continue;
        }
        match mode {
            Mode::Generate => {
                write(root, &target, &rendered)?;
                report.written.push(display(&target));
            }
            Mode::Diff => report.stale.push(display(&target)),
            Mode::Validate => unreachable!("returned above"),
        }
    }

    Ok(report)
}

/// Imports one Smart Data Models model and writes the LinkML source it becomes (DM-07…DM-11).
///
/// No pin is checked: this writes an authoring source a person then reviews and commits, not
/// an artifact CI compares, and the command takes a path rather than a repository.
pub fn import(tools: &ModelTools, model: &str, out: &Path) -> Result<Value, Error> {
    let answer = tools.import_sdm(model)?;
    let source = match answer.artifacts.get("linkml") {
        Some(Value::String(source)) => source.clone(),
        _ if !answer.errors.is_empty() => String::new(),
        _ => {
            return Err(http::HttpError::Protocol(
                "an import answer with no `linkml`, so there is no source to write".into(),
            )
            .into())
        }
    };

    if !source.is_empty() {
        if let Some(parent) = out.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).map_err(|source| Error::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        std::fs::write(out, &source).map_err(|source| Error::Io {
            path: out.to_path_buf(),
            source,
        })?;
    }

    Ok(serde_json::json!({
        "generatorVersion": answer.generator_version,
        "model": model,
        "written": if source.is_empty() { Vec::new() } else { vec![display(out)] },
        "errors": answer.errors,
    }))
}

/// One artifact as the bytes that go in the repository.
///
/// A generator that answers with a string (SHACL, OWL, Markdown) has already decided how its
/// artifact looks; anything else is JSON and is written pretty-printed, so a review sees the
/// change rather than one reflowed line. `generate` writes exactly what `diff` compares, which
/// is what makes byte equality the whole of DM-02's check.
fn render(value: &Value) -> String {
    match value {
        Value::String(text) if text.ends_with('\n') => text.clone(),
        Value::String(text) => format!("{text}\n"),
        other => format!(
            "{}\n",
            serde_json::to_string_pretty(other).unwrap_or_else(|_| other.to_string())
        ),
    }
}

fn read(root: &Path, relative: &Path) -> Result<String, Error> {
    std::fs::read_to_string(root.join(relative)).map_err(|source| Error::Io {
        path: relative.to_path_buf(),
        source,
    })
}

fn write(root: &Path, relative: &Path, contents: &str) -> Result<(), Error> {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| Error::Io {
            path: relative.to_path_buf(),
            source,
        })?;
    }
    std::fs::write(&path, contents).map_err(|source| Error::Io {
        path: relative.to_path_buf(),
        source,
    })
}

/// Repository-relative paths are printed with forward slashes on every platform.
fn display(path: &Path) -> String {
    path.components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

/// One entry of an `errors` array; the service sends strings, and anything else is shown raw.
fn render_message(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// Enough of an unexpected body to recognise it, without pasting a model into a log.
fn truncate(body: &str) -> String {
    let body = body.trim();
    match body.char_indices().nth(200) {
        Some((cut, _)) => format!("{}…", &body[..cut]),
        None => body.to_owned(),
    }
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Generate => "generate",
            Self::Diff => "diff",
            Self::Validate => "validate",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_string_artifact_is_written_as_it_is_and_a_json_one_pretty_printed() {
        assert_eq!(render(&Value::String("# Docs".into())), "# Docs\n");
        assert_eq!(render(&Value::String("# Docs\n".into())), "# Docs\n");
        assert_eq!(
            render(&serde_json::json!({"b": 1, "a": 2})),
            "{\n  \"a\": 2,\n  \"b\": 1\n}\n"
        );
    }

    #[test]
    fn an_answer_splits_into_the_version_the_artifacts_and_the_errors() {
        let answer = Answer::parse(
            r#"{"generatorVersion":"linkml-1.11.1","jsonSchema":{"x":1},"errors":["nope"]}"#,
        )
        .unwrap();
        assert_eq!(answer.generator_version, "linkml-1.11.1");
        assert_eq!(answer.errors, vec!["nope".to_owned()]);
        assert_eq!(answer.artifacts.keys().collect::<Vec<_>>(), ["jsonSchema"]);
    }

    #[test]
    fn an_answer_that_does_not_say_what_rendered_it_is_refused() {
        let err = Answer::parse(r#"{"jsonSchema":{}}"#).unwrap_err();
        assert!(err.to_string().contains("generatorVersion"), "{err}");
    }

    #[test]
    fn a_repository_with_no_settings_file_cannot_be_generated_into() {
        let dir = std::env::temp_dir().join("jcctl-model-no-settings");
        std::fs::create_dir_all(&dir).unwrap();
        let err = pinned_generator(&dir).unwrap_err();
        assert!(err.to_string().contains("DM-19"), "{err}");
    }

    #[test]
    fn the_pin_is_read_and_the_other_settings_are_left_alone() {
        let dir = std::env::temp_dir().join("jcctl-model-settings");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(SETTINGS_FILE),
            "retention:\n  history: 90d\nmodelTools:\n  image: ghcr.io/x@sha256:0\n  generatorVersion: linkml-1.11.1\n",
        )
        .unwrap();
        assert_eq!(pinned_generator(&dir).unwrap(), "linkml-1.11.1");
    }

    #[test]
    fn a_settings_file_without_the_block_says_so_rather_than_defaulting() {
        let dir = std::env::temp_dir().join("jcctl-model-settings-empty");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(SETTINGS_FILE), "retention:\n  history: 90d\n").unwrap();
        let err = pinned_generator(&dir).unwrap_err();
        assert!(
            err.to_string().contains("modelTools.generatorVersion"),
            "{err}"
        );
    }
}
