//! The kubectl-shaped verbs: `get`, `describe`, `apply -f`, `diff -f` and `delete -f` as a client
//! of the Portal resource API (MF-14, API/03 §2a, API/01 §4).
//!
//! Every write is a proposal: the Portal answers `202` with a `Change` that waits for a person's
//! approval, and a deletion goes to the deletion lane (MF-12, CC-41). Nothing here writes live
//! state or a repository. The token is the only credential, read from a file, sent in the
//! `Authorization` header and replaced by `[redacted]` in anything an answer echoes back.

use crate::diff::diff;
use crate::loader::{is_empty_doc, parse_yaml_documents, RawManifest};
use crate::secrets::SecretValue;
use reqwest::blocking::{Client, RequestBuilder, Response};
use reqwest::{StatusCode, Url};
use serde_json::Value;
use std::fmt::Write as _;
use std::time::Duration;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
/// How much of an answer that is not problem JSON an error repeats.
const SNIPPET: usize = 200;
/// Pages a list follows before it stops: a `continue` that never ends is the server's bug, not
/// a reason to loop for ever.
const MAX_PAGES: usize = 1000;

/// Why a verb could not do what it was asked.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PortalError {
    /// The Portal could not be reached, or its address is not usable.
    #[error("the Portal at {url} could not be reached: {message}")]
    Unavailable {
        /// The address that was tried, never the credential.
        url: String,
        /// What went wrong.
        message: String,
    },
    /// The Portal answered, and refused.
    #[error("{what}: the Portal refused ({status}): {detail}")]
    Refused {
        /// The resource the call was for, `{plural}/{name}` or `{plural}`.
        what: String,
        /// The status it answered.
        status: u16,
        /// Its `detail`, or the start of a body that is not problem JSON.
        detail: String,
    },
    /// The input names nothing the Portal serves, or disagrees with itself.
    #[error("{0}")]
    Invalid(String),
}

/// How `get` prints what it read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Output {
    /// `{plural}/{name}`, one per line (the default).
    Name,
    /// The manifest, or the `List`, as YAML.
    Yaml,
    /// The same as JSON.
    Json,
}

impl Output {
    /// The value of `-o`.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "name" => Some(Self::Name),
            "yaml" => Some(Self::Yaml),
            "json" => Some(Self::Json),
            _ => None,
        }
    }
}

/// The Portal resource API, reached with one token.
#[derive(Debug)]
pub struct Portal {
    base: Url,
    token: SecretValue,
    client: Client,
}

impl Portal {
    /// A client for the Portal at `base_url`. A URL carrying a user name or password is refused:
    /// the token is the one credential and it travels in a header.
    pub fn new(base_url: &str, token: SecretValue) -> Result<Self, PortalError> {
        let unusable = |message: &str| PortalError::Unavailable {
            url: base_url.to_owned(),
            message: message.to_owned(),
        };
        let base = Url::parse(base_url.trim_end_matches('/'))
            .map_err(|e| unusable(&format!("it is not a URL: {e}")))?;
        if !base.username().is_empty() || base.password().is_some() {
            return Err(unusable(
                "it carries credentials; the token is the only credential and it travels in the \
                 Authorization header",
            ));
        }
        if !matches!(base.scheme(), "http" | "https") || base.host_str().is_none() {
            return Err(unusable("it is not an http(s) URL naming a host"));
        }
        if token.is_empty() {
            return Err(unusable("the token file is empty"));
        }
        let client = Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .user_agent("jcctl")
            .build()
            .map_err(|e| unusable(&e.to_string()))?;
        Ok(Self {
            base,
            token,
            client,
        })
    }

    /// `/api/v1/projects/{project}/{plural}[/{name}]`, each segment escaped, so a name can
    /// never climb out of its collection.
    fn url(&self, project: &str, plural: &str, name: Option<&str>) -> Url {
        let mut url = self.base.clone();
        if let Ok(mut path) = url.path_segments_mut() {
            path.pop_if_empty()
                .extend(["api", "v1", "projects", project, plural]);
            if let Some(name) = name {
                path.push(name);
            }
        }
        url
    }

    fn send(
        &self,
        what: &str,
        request: RequestBuilder,
    ) -> Result<(StatusCode, String), PortalError> {
        let response: Response = request
            .bearer_auth(self.token.expose())
            .header("Accept", "application/json")
            .send()
            .map_err(|e| PortalError::Unavailable {
                url: self.base.to_string(),
                // `reqwest` repeats the URL and nothing of the headers.
                message: e.without_url().to_string(),
            })?;
        let status = response.status();
        let body = response.text().map_err(|e| PortalError::Unavailable {
            url: self.base.to_string(),
            message: format!("{what}: the answer could not be read: {e}"),
        })?;
        Ok((status, body))
    }

    fn refused(&self, what: &str, status: StatusCode, body: &str) -> PortalError {
        let detail = serde_json::from_str::<Value>(body)
            .ok()
            .and_then(|v| v.get("detail").and_then(Value::as_str).map(str::to_owned))
            .unwrap_or_else(|| snippet(body));
        PortalError::Refused {
            what: what.to_owned(),
            status: status.as_u16(),
            detail: detail.replace(self.token.expose(), "[redacted]"),
        }
    }

    fn json(&self, what: &str, body: &str) -> Result<Value, PortalError> {
        serde_json::from_str(body).map_err(|_| PortalError::Refused {
            what: what.to_owned(),
            status: 200,
            detail: format!(
                "the answer is not JSON: {}",
                snippet(&body.replace(self.token.expose(), "[redacted]"))
            ),
        })
    }

    /// Every manifest of a collection, following `continue` to the end.
    pub fn list(
        &self,
        project: &str,
        plural: &str,
        label_selector: Option<&str>,
    ) -> Result<Vec<Value>, PortalError> {
        let mut items = Vec::new();
        let mut next: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let mut url = self.url(project, plural, None);
            {
                let mut query = url.query_pairs_mut();
                if let Some(selector) = label_selector {
                    query.append_pair("labelSelector", selector);
                }
                if let Some(token) = &next {
                    query.append_pair("continue", token);
                }
            }
            let (status, body) = self.send(plural, self.client.get(url))?;
            if status != StatusCode::OK {
                return Err(self.refused(plural, status, &body));
            }
            let page = self.json(plural, &body)?;
            match page.get("items") {
                Some(Value::Array(page_items)) => items.extend(page_items.iter().cloned()),
                _ => {
                    return Err(PortalError::Refused {
                        what: plural.to_owned(),
                        status: 200,
                        detail: "the answer is not a List".to_owned(),
                    })
                }
            }
            next = page
                .pointer("/metadata/continue")
                .and_then(Value::as_str)
                .filter(|t| !t.is_empty())
                .map(str::to_owned);
            if next.is_none() {
                return Ok(items);
            }
        }
        Err(PortalError::Refused {
            what: plural.to_owned(),
            status: 200,
            detail: format!("the list did not end after {MAX_PAGES} pages"),
        })
    }

    /// One manifest, or `None` when the Portal holds none this caller may read (the one answer
    /// for both, R20).
    pub fn get(
        &self,
        project: &str,
        plural: &str,
        name: &str,
    ) -> Result<Option<Value>, PortalError> {
        let what = format!("{plural}/{name}");
        let (status, body) = self.send(
            &what,
            self.client.get(self.url(project, plural, Some(name))),
        )?;
        match status {
            StatusCode::OK => self.json(&what, &body).map(Some),
            StatusCode::NOT_FOUND => Ok(None),
            _ => Err(self.refused(&what, status, &body)),
        }
    }

    /// Proposes `manifest`: `POST` to the collection when `replace` is false, `PUT` to the
    /// resource when it is true. The answer is the `Change`.
    pub fn propose(
        &self,
        project: &str,
        plural: &str,
        name: &str,
        manifest: &Value,
        replace: bool,
    ) -> Result<Value, PortalError> {
        let what = format!("{plural}/{name}");
        let request = if replace {
            self.client.put(self.url(project, plural, Some(name)))
        } else {
            self.client.post(self.url(project, plural, None))
        };
        self.change(&what, request.json(manifest))
    }

    /// Proposes the deletion of one resource. The answer is the `Change`.
    pub fn delete(&self, project: &str, plural: &str, name: &str) -> Result<Value, PortalError> {
        let what = format!("{plural}/{name}");
        self.change(
            &what,
            self.client.delete(self.url(project, plural, Some(name))),
        )
    }

    fn change(&self, what: &str, request: RequestBuilder) -> Result<Value, PortalError> {
        let (status, body) = self.send(what, request)?;
        if status != StatusCode::ACCEPTED {
            return Err(self.refused(what, status, &body));
        }
        self.json(what, &body)
    }
}

/// The first `SNIPPET` characters of an answer.
fn snippet(body: &str) -> String {
    let trimmed = body.trim();
    match trimmed.char_indices().nth(SNIPPET) {
        Some((at, _)) => format!("{}…", &trimmed[..at]),
        None => trimmed.to_owned(),
    }
}

/// The plural a collection is served under, checked against the kind registry.
fn known_plural(plural: &str) -> Result<(), PortalError> {
    jc_core::by_plural(plural).map(|_| ()).ok_or_else(|| {
        let known: Vec<&str> = jc_core::KINDS.iter().map(|k| k.plural).collect();
        PortalError::Invalid(format!(
            "no kind is served as '{plural}'; the plurals are {}",
            known.join(", ")
        ))
    })
}

fn name_of(manifest: &Value) -> &str {
    manifest
        .pointer("/metadata/name")
        .and_then(Value::as_str)
        .unwrap_or("?")
}

/// `jcctl get <plural> [<name>] --project <p> [-o …] [-l …]`.
pub fn get(
    portal: &Portal,
    project: &str,
    plural: &str,
    name: Option<&str>,
    output: Output,
    label_selector: Option<&str>,
) -> Result<String, PortalError> {
    known_plural(plural)?;
    let (items, single) = match name {
        Some(name) => match portal.get(project, plural, name)? {
            Some(manifest) => (vec![manifest], true),
            None => {
                return Err(PortalError::Invalid(format!(
                    "{plural}/{name}: not found in project {project}"
                )))
            }
        },
        None => (portal.list(project, plural, label_selector)?, false),
    };
    let shown = if single {
        items[0].clone()
    } else {
        serde_json::json!({ "apiVersion": "v1", "kind": "List", "items": items })
    };
    Ok(match output {
        Output::Name => items
            .iter()
            .map(|m| format!("{plural}/{}\n", name_of(m)))
            .collect(),
        Output::Json => format!("{}\n", pretty_json(&shown)),
        Output::Yaml => yaml(&shown)?,
    })
}

/// `jcctl describe <plural> <name> --project <p>`: identity and title, then `spec` and `status`.
pub fn describe(
    portal: &Portal,
    project: &str,
    plural: &str,
    name: &str,
) -> Result<String, PortalError> {
    known_plural(plural)?;
    let manifest = portal.get(project, plural, name)?.ok_or_else(|| {
        PortalError::Invalid(format!("{plural}/{name}: not found in project {project}"))
    })?;
    let text = |pointer: &str| {
        manifest
            .pointer(pointer)
            .and_then(Value::as_str)
            .unwrap_or("")
    };
    let mut out = String::new();
    let _ = writeln!(out, "Kind:     {}", text("/kind"));
    let _ = writeln!(out, "Name:     {}", name_of(&manifest));
    let _ = writeln!(out, "Project:  {project}");
    for (label, pointer) in [
        ("Title:    ", "/metadata/title"),
        ("Description: ", "/metadata/description"),
    ] {
        if !text(pointer).is_empty() {
            let _ = writeln!(out, "{label}{}", text(pointer));
        }
    }
    for (label, key) in [("Spec", "spec"), ("Status", "status")] {
        if let Some(value) = manifest.get(key).filter(|v| !v.is_null()) {
            let _ = writeln!(out, "{label}:");
            for line in yaml(value)?.lines() {
                let _ = writeln!(out, "  {line}");
            }
        }
    }
    Ok(out)
}

fn pretty_json(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

fn yaml(value: &Value) -> Result<String, PortalError> {
    serde_norway::to_string(value)
        .map_err(|e| PortalError::Invalid(format!("the answer does not print as YAML: {e}")))
}

/// One manifest of a `-f` file, with where its request goes.
#[derive(Debug)]
pub struct Target {
    /// The project whose collection it belongs to.
    pub project: String,
    /// The kind's plural.
    pub plural: &'static str,
    /// The manifest as the file declares it, without `status`.
    pub manifest: RawManifest,
}

/// Reads the manifests of a `-f` file and where each goes: the project is `--project`, else the
/// manifest's `metadata.namespace`, and a manifest naming another project than `--project` is
/// refused before anything is sent, as is an unknown kind. `status` is the server's (CC-17) and
/// is dropped.
pub fn targets(text: &str, project: Option<&str>) -> Result<Vec<Target>, PortalError> {
    let mut out = Vec::new();
    for chunk in parse_yaml_documents(text) {
        if is_empty_doc(&chunk.content) {
            continue;
        }
        let at = format!(
            "document {} (line {})",
            chunk.document_index, chunk.start_line
        );
        let mut manifest: RawManifest = serde_norway::from_str(&chunk.content)
            .map_err(|e| PortalError::Invalid(format!("{at}: not a manifest: {e}")))?;
        manifest.status = None;
        let kind = jc_core::by_kind(&manifest.kind)
            .ok_or_else(|| PortalError::Invalid(format!("{at}: no kind '{}'", manifest.kind)))?;
        let declared = manifest.metadata.namespace.as_deref();
        let project = match (project, declared) {
            (Some(flag), Some(ns)) if flag != ns => {
                return Err(PortalError::Invalid(format!(
                    "{at}: {}/{} is in project {ns}, not {flag}",
                    kind.plural, manifest.metadata.name
                )))
            }
            (Some(p), _) | (None, Some(p)) => p.to_owned(),
            (None, None) => {
                return Err(PortalError::Invalid(format!(
                    "{at}: {}/{} names no project: set metadata.namespace or pass --project",
                    kind.plural, manifest.metadata.name
                )))
            }
        };
        out.push(Target {
            project,
            plural: kind.plural,
            manifest,
        });
    }
    if out.is_empty() {
        return Err(PortalError::Invalid(
            "the file holds no manifest".to_owned(),
        ));
    }
    Ok(out)
}

/// `{Change name} {phase} ({lane} lane)`, from the `Change` a write answered.
fn change_line(change: &Value) -> String {
    let field = |pointer: &str| {
        change
            .pointer(pointer)
            .and_then(Value::as_str)
            .unwrap_or("?")
    };
    format!(
        "{} {} ({} lane)",
        field("/metadata/name"),
        field("/status/phase"),
        field("/status/lane")
    )
}

/// What a verb over a file did: one line per manifest, and whether every one went through.
#[derive(Debug, Default)]
pub struct Report {
    /// One line per manifest.
    pub lines: Vec<String>,
    /// Whether any manifest was refused.
    pub failed: bool,
    /// Whether any manifest differs from the live one (`diff -f`).
    pub differs: bool,
}

impl Report {
    fn refused(&mut self, error: &PortalError) {
        self.lines.push(format!("error: {error}"));
        self.failed = true;
    }
}

fn live_manifest(live: Value) -> Result<RawManifest, PortalError> {
    serde_json::from_value(live)
        .map_err(|e| PortalError::Invalid(format!("the live manifest does not read: {e}")))
}

/// `jcctl apply -f`: proposes every manifest that differs from the live one, `POST` for a new
/// resource and `PUT` for a changed one; a manifest the Portal already holds as declared opens
/// no `Change`. A refusal is reported and the next manifest still goes.
pub fn apply(portal: &Portal, targets: &[Target]) -> Report {
    let mut report = Report::default();
    for target in targets {
        let name = &target.manifest.metadata.name;
        let what = format!("{}/{name}", target.plural);
        let result = portal
            .get(&target.project, target.plural, name)
            .and_then(|live| {
                let replace = match live {
                    Some(live) => {
                        if diff(&target.manifest, &live_manifest(live)?).is_empty() {
                            return Ok(format!("{what}: unchanged"));
                        }
                        true
                    }
                    None => false,
                };
                let body = serde_json::to_value(&target.manifest)
                    .map_err(|e| PortalError::Invalid(format!("{what}: {e}")))?;
                let change =
                    portal.propose(&target.project, target.plural, name, &body, replace)?;
                let verb = if replace { "replaced" } else { "created" };
                Ok(format!("{what}: {verb}, {}", change_line(&change)))
            });
        match result {
            Ok(line) => report.lines.push(line),
            Err(e) => report.refused(&e),
        }
    }
    report
}

/// `jcctl diff -f`: every member the file declares that the live manifest does not match, as
/// `plan` compares them (CC-17, CC-69). Writes nothing.
pub fn diff_file(portal: &Portal, targets: &[Target]) -> Report {
    let mut report = Report::default();
    for target in targets {
        let name = &target.manifest.metadata.name;
        let what = format!("{}/{name}", target.plural);
        match portal.get(&target.project, target.plural, name) {
            Ok(None) => {
                report.differs = true;
                report
                    .lines
                    .push(format!("{what}: not in the Portal, apply would create it"));
            }
            Ok(Some(live)) => match live_manifest(live) {
                Ok(live) => {
                    let fields = diff(&target.manifest, &live);
                    if fields.is_empty() {
                        report.lines.push(format!("{what}: unchanged"));
                    }
                    for field in fields {
                        report.differs = true;
                        let shown = |v: &Option<Value>| {
                            v.as_ref().map_or("(absent)".to_owned(), Value::to_string)
                        };
                        report.lines.push(format!(
                            "{what}: {}: {} -> {}",
                            field.path,
                            shown(&field.live),
                            shown(&field.declared)
                        ));
                    }
                }
                Err(e) => report.refused(&e),
            },
            Err(e) => report.refused(&e),
        }
    }
    report
}

/// `jcctl delete -f`: proposes the deletion of every resource the file names (MF-12).
pub fn delete(portal: &Portal, targets: &[Target]) -> Report {
    let mut report = Report::default();
    for target in targets {
        let name = &target.manifest.metadata.name;
        match portal.delete(&target.project, target.plural, name) {
            Ok(change) => report.lines.push(format!(
                "{}/{name}: deleted, {}",
                target.plural,
                change_line(&change)
            )),
            Err(e) => report.refused(&e),
        }
    }
    report
}
