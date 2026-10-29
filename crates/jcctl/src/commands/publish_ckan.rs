//! `jcctl publish ckan --repo-dir <path> --project <p> --host <gateway host>` (T-0487,
//! EP-62…EP-67, CC-18).
//!
//! The command walks the configuration repository the way `apply` does, takes every
//! Endpoint of one project that declares `spec.publish.ckan`, resolves the `CkanInstance`
//! it names and the API token behind the instance's `apiTokenRef`, reads the Endpoint's
//! own DCAT-AP record from the gateway and hands all of that to
//! [`crate::publish::ckan::publish`]. The schemas its charts are built from come through the
//! same gateway, as any consumer would read them (EP-66). CKAN holds no rows (T-5060).
//!
//! Nothing here is authored a second time: the dataset is the record, the resources are
//! the enabled representations, and a second run over an unchanged repository makes no
//! writing call to the catalogue (CC-18). The token is read here and handed to the HTTP
//! client; it is never printed, and never part of an error (EP-67).

use crate::loader::{LoadError, RawManifest, Repository, ResourceId};
use crate::publish::ckan::{self, CkanApi, Outcome, PublishError, Settings};
use crate::secrets::{identities_from_file, SecretStore, SecretValue};
use jc_core::envelope::{Kind, Ref};
use jc_core::i18n::Text;
use jc_core::kinds::ckan::{CkanInstanceSpec, CkanPublication};
use jc_core::kinds::policy::{Operation, OperationRef, PolicySpec, PrincipalKind};
use jc_core::kinds::{ContextSpaceSpec, EndpointSpec};
use serde_json::{json, Value};
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
    /// The instance's `metadata.title` in that language: what its organization is called,
    /// created or renamed to, before the installation's branding name (ADR-N-058 2.6).
    pub instance_title: Option<String>,
    /// What the public may read of the Endpoint's history: its automatic charts (T-5421).
    pub history: ckan::History,
}

impl Target {
    /// The CKAN dataset name this Endpoint publishes as.
    pub fn dataset_name(&self) -> &str {
        self.publication.dataset_name(&self.id.name)
    }

    /// `settings` for this Endpoint: its space's language, and its instance's title, which
    /// the organization is created with or renamed to.
    fn settings(&self, settings: &Settings) -> Settings {
        let mut own = settings.clone();
        if let Some(language) = &self.language {
            own = own.in_language(language.clone());
        }
        if let Some(title) = &self.instance_title {
            own = own.declared_title(title.clone());
        }
        own
    }
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
    /// The published record lists a model without its LinkML artifact, so the dataset links the
    /// schema index for it (EP-68).
    pub no_linkml: bool,
    /// What happened to the dataset's visualization views (EP-132).
    pub views: ckan::Views,
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
        let views = &self.views;
        if *views != ckan::Views::default() {
            write!(
                f,
                ", views {} created, {} updated, {} deleted",
                views.created, views.updated, views.deleted
            )?;
        }
        if self.no_linkml {
            write!(
                f,
                "; the record lists no model.linkml.yaml, the dataset links /schema/index.json only"
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
    /// The publisher refused or CKAN did.
    #[error(transparent)]
    Publish(#[from] PublishError),
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
        let space = space_of(repo, id, &spec);
        let language = space
            .as_ref()
            .and_then(|space| space.default_locale.clone())
            .filter(|locale| !locale.trim().is_empty());
        let history = history(repo, id, &spec, space.as_ref());
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
            history,
        });
    }
    Ok(targets)
}

/// The space an Endpoint serves, looked up in the Endpoint's project.
fn space_of(
    repo: &Repository,
    endpoint: &ResourceId,
    spec: &EndpointSpec,
) -> Option<ContextSpaceSpec> {
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
    serde_json::from_value(repo.get(&space)?.manifest.spec.clone()).ok()
}

/// What the public may read of the history an Endpoint serves (T-5421): the `queryTemporal`
/// grants to the public role in the Policies of its project and space it binds (all of them
/// when it names none, ADR-N-057), and the space's retention.
fn history(
    repo: &Repository,
    endpoint: &ResourceId,
    spec: &EndpointSpec,
    space: Option<&ContextSpaceSpec>,
) -> ckan::History {
    let bound = |name: &str| {
        spec.policy_refs()
            .is_none_or(|refs| refs.iter().any(|r| r.local_id() == name))
    };
    let grants = repo
        .iter()
        .filter(|(id, _)| {
            id.kind == PolicySpec::KIND && id.namespace == endpoint.namespace && bound(&id.name)
        })
        .filter_map(|(id, resource)| {
            let policy: PolicySpec = serde_json::from_value(resource.manifest.spec.clone()).ok()?;
            policy.expand(&id.name).ok()
        })
        .flatten()
        .filter(|(_, policy)| policy.context_space_ref.name() == spec.context_space_ref.name())
        .flat_map(|(_, policy)| temporal_grants(&policy))
        .collect();
    ckan::History {
        grants,
        retention: space.map(|s| s.retention.clone()).unwrap_or_default(),
    }
}

/// The public `queryTemporal` grants of one single-rule Policy, one per `information` entry. A
/// `temporalQ` other than a moving window (`timerel=after;timeAt=P-30D`) grants no chart: a fixed
/// period would leave the chart empty once it is over.
fn temporal_grants(policy: &PolicySpec) -> Vec<ckan::TemporalGrant> {
    let temporal = |op: &OperationRef| match op {
        OperationRef::Single(op) => *op == Operation::QueryTemporal,
        OperationRef::Group(group) => group.operations().contains(&Operation::QueryTemporal),
    };
    let public = policy.assignee.kind == PrincipalKind::Role && policy.assignee.id == "public";
    let seconds = policy.temporal_q.as_deref().map(|q| {
        q.strip_prefix("timerel=after;timeAt=P-")
            .and_then(|back| jc_core::kinds::space::duration_seconds(&format!("P{back}")))
    });
    if !policy.effect.is_permission() || !public || !policy.operations.iter().any(temporal) {
        return Vec::new();
    }
    let Some(seconds) = seconds.map_or(Some(None), |window| window.map(Some)) else {
        return Vec::new();
    };
    if policy.information.is_empty() {
        return vec![ckan::TemporalGrant {
            seconds,
            ..Default::default()
        }];
    }
    policy
        .information
        .iter()
        .map(|info| ckan::TemporalGrant {
            types: info
                .entities
                .iter()
                .map(|e| e.entity_type.clone())
                .collect(),
            attrs: info.property_names.clone(),
            seconds,
        })
        .collect()
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

/// The prefix of every environment variable `apiTokenRef.envVar` may name.
const CKAN_ENV_PREFIX: &str = "CKAN_";

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
    // The manifest names the variable and its own `baseUrl` receives the value, so a manifest
    // naming `HOME` or another project's secret would send that out; only a `CKAN_` variable
    // is a CKAN token (F-PLT3-009).
    if let Some(variable) = reference.env_var.as_deref() {
        if !variable.starts_with(CKAN_ENV_PREFIX) {
            return Err(fail(format!(
                "apiTokenRef.envVar names {variable}, and only a {CKAN_ENV_PREFIX}… variable \
                 holds a CKAN token: rename it, or pass --api-token-env <VAR>"
            )));
        }
        if let Some(value) = SecretValue::from_env(variable) {
            return Ok(value);
        }
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
        return Err(gateway(refusal(status, &text)));
    }
    Ok(text)
}

/// What a refused read says: the status and, for a problem document, its `detail` (else its
/// `title`), so the Portal's CKAN page shows words instead of the document's JSON (T-5748).
/// Another JSON body says only the status's reason; a text body its first 200 characters.
fn refusal(status: reqwest::StatusCode, text: &str) -> String {
    let code = status.as_u16();
    let reason = status.canonical_reason().unwrap_or("unexpected status");
    let shown = match serde_json::from_str::<Value>(text) {
        Ok(body) => ["detail", "title"]
            .iter()
            .find_map(|key| body.get(key).and_then(Value::as_str))
            .unwrap_or(reason)
            .to_owned(),
        Err(_) => text.split_whitespace().collect::<Vec<_>>().join(" "),
    };
    let shown: String = shown.chars().take(200).collect();
    if shown.is_empty() {
        format!("{code} {reason}")
    } else {
        format!("{code} {shown}")
    }
}

/// The DCAT-AP record of one target, read from the gateway (EP-27, EP-63), with the entity type
/// the Endpoint's WebSocket guide names under [`ckan::EXAMPLE_TYPE`] when it names one (T-4132).
pub fn record(target: &Target, settings: &Settings) -> Result<Value, Error> {
    let base = settings.endpoint_url(&target.slug);
    let url = format!("{base}/");
    let text = fetch(&url, "application/ld+json")?;
    let mut record: Value = serde_json::from_str(&text).map_err(|e| Error::Gateway {
        url,
        message: format!("the record is not JSON: {e}"),
    })?;
    let (kind, served) = example_type(&base)?;
    with_types(&mut record, kind, served);
    Ok(record)
}

/// The type a usage text names (T-4132): the one the gateway's `426` guide of `{endpoint}/ws`
/// subscribes its example to, when the Endpoint's own `/ngsi-ld/v1/types` lists it, else the
/// first type that list names (T-4802: the guide of the Bridge-only bbsk-mosty named
/// AdministrativeArea, so its dataset page sent readers, and the assistant, to a type it does
/// not serve). `None` when neither answers: the texts then say how to list the types. With it
/// the types the list names, `None` when it could not be read.
///
/// A read the gateway refused for load (429), failed (5xx) or never answered is an error: the
/// Endpoint's types are unknown only for that moment, and a page written without them sent the
/// assistant to no dataset until the next refresh (T-6091: every dataset republished at once
/// after a Portal restart, bbsk-zariadenia lost its typed addresses for an hour).
fn example_type(base: &str) -> Result<(Option<String>, Option<Vec<String>>), Error> {
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(FETCH_TIMEOUT)
        .user_agent(ckan::GENERATOR)
        .build()
        .map_err(|e| Error::Gateway {
            url: base.to_owned(),
            message: format!("HTTP client: {e}"),
        })?;
    let read = |path: &str, expected: reqwest::StatusCode| -> Result<Option<Value>, Error> {
        let url = format!("{base}/{path}");
        let gateway = |message: String| Error::Gateway {
            url: url.clone(),
            message,
        };
        let response = client
            .get(&url)
            .header("Accept", "application/json")
            .send()
            .map_err(|e| gateway(e.to_string()))?;
        let status = response.status();
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
            let text = response.text().unwrap_or_default();
            return Err(gateway(refusal(status, &text)));
        }
        Ok((status == expected).then(|| response.json().ok()).flatten())
    };
    let guide = read("ws", reqwest::StatusCode::UPGRADE_REQUIRED)?.and_then(|g| guide_type(&g));
    let served = read("ngsi-ld/v1/types", reqwest::StatusCode::OK)?.map(|list| served_types(&list));
    Ok((example_of(guide, served.clone()), served))
}

/// `record` with the private members the usage texts read: the example type and the types the
/// Endpoint serves, each only when known.
fn with_types(record: &mut Value, kind: Option<String>, served: Option<Vec<String>>) {
    let Some(members) = record.as_object_mut() else {
        return;
    };
    if let Some(kind) = kind {
        members.insert(ckan::EXAMPLE_TYPE.to_owned(), Value::String(kind));
    }
    if let Some(served) = served.filter(|served| !served.is_empty()) {
        members.insert(ckan::SERVED_TYPES.to_owned(), json!(served));
    }
}

/// The short names of an NGSI-LD `EntityTypeList`; none for any other answer.
fn served_types(list: &Value) -> Vec<String> {
    list.get("typeList")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|kind| !kind.is_empty())
        .map(str::to_owned)
        .collect()
}

/// The guide's type when the Endpoint serves it, else the first it serves; the guide's alone
/// when the type list could not be read.
fn example_of(guide: Option<String>, served: Option<Vec<String>>) -> Option<String> {
    match served {
        Some(served) => match guide {
            Some(kind) if served.contains(&kind) => Some(kind),
            _ => served.into_iter().next(),
        },
        None => guide,
    }
}

/// The type of a guide's example subscription.
fn guide_type(guide: &Value) -> Option<String> {
    let kind = guide.pointer("/example/body/entities/0/type")?.as_str()?;
    (!kind.is_empty()).then(|| kind.to_owned())
}

/// The JSON Schema and LinkML of one Endpoint's models: what its automatic charts are built
/// from (EP-132, T-5421).
#[derive(Debug, Clone, PartialEq)]
pub struct Models {
    /// The Endpoint's `model.schema.json`, narrowed to the anonymous grant.
    pub schema: Value,
    /// The Endpoint's `model.linkml.yaml`, the display order and labels of its charts (T-4672);
    /// `None` leaves them to the name heuristic.
    pub linkml: Option<String>,
}

/// Reads the models of one target through its own Endpoint (EP-66, T-5060).
pub fn models(target: &Target, settings: &Settings) -> Result<Models, Error> {
    let base = settings.endpoint_url(&target.slug);
    let url = format!("{base}/schema/v1/model.schema.json");
    let text = fetch(&url, "application/schema+json")?;
    let schema = serde_json::from_str(&text).map_err(|e| Error::Gateway {
        url,
        message: format!("the schema is not JSON: {e}"),
    })?;
    // A gateway that serves no LinkML leaves the charts to the heuristic (DM-98).
    let linkml = fetch(&format!("{base}/schema/v1/model.linkml.yaml"), "text/yaml").ok();
    Ok(Models { schema, linkml })
}

/// Publishes one Endpoint: the dataset and its views (EP-62, EP-132).
///
/// `models` are the schemas the automatic charts are built from, `None` for no automatic
/// charts. A dataset that already matches makes no writing call (CC-18).
pub fn publish_one(
    api: &mut impl CkanApi,
    target: &Target,
    record: &Value,
    models: Option<&Models>,
    settings: &Settings,
) -> Result<Line, Error> {
    let settings = target.settings(settings);
    let outcome = ckan::publish(api, &target.manifest, &target.instance, record, &settings)?;
    let views = match outcome {
        Outcome::NotPublished => ckan::Views::default(),
        _ => {
            let automatic = models
                .map(|m| ckan::series_charts(&m.schema, m.linkml.as_deref(), &target.history))
                .unwrap_or_default();
            ckan::publish_views(api, &target.manifest, &settings, &automatic)?
        }
    };
    Ok(Line {
        endpoint: target.id.clone(),
        dataset: target.dataset_name().to_owned(),
        outcome,
        no_linkml: outcome != Outcome::NotPublished && ckan::missing_linkml(record),
        views,
    })
}

/// Withdraws one Endpoint's dataset (EP-62, CC-19).
///
/// Withdrawing what is not there succeeds, so a run can be repeated after a partial
/// failure.
pub fn withdraw_one(api: &mut impl CkanApi, target: &Target) -> Result<Line, Error> {
    let name = target.dataset_name().to_owned();
    let outcome = ckan::withdraw(api, &name)?;
    Ok(Line {
        endpoint: target.id.clone(),
        dataset: name,
        outcome,
        no_linkml: false,
        views: ckan::Views::default(),
    })
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

#[cfg(test)]
mod tests {
    use super::{example_of, example_type, guide_type, refusal, served_types, with_types};
    use reqwest::StatusCode;
    use serde_json::json;

    /// T-5748: a refused read names the problem's detail, never the problem document's JSON.
    #[test]
    fn a_refusal_says_the_problem_detail_not_its_json() {
        let problem = r#"{"type":"https://uri.etsi.org/ngsi-ld/errors/ResourceNotFound","title":"Not found","detail":"the view helsinki-bikes lists no file.csv"}"#;
        assert_eq!(
            refusal(StatusCode::NOT_FOUND, problem),
            "404 the view helsinki-bikes lists no file.csv"
        );
        assert_eq!(
            refusal(StatusCode::NOT_FOUND, r#"{"title":"Not found"}"#),
            "404 Not found"
        );
        assert_eq!(
            refusal(StatusCode::BAD_GATEWAY, r#"{"error":{"code":7}}"#),
            "502 Bad Gateway"
        );
        assert_eq!(
            refusal(StatusCode::SERVICE_UNAVAILABLE, "  upstream\n  timed out "),
            "503 upstream timed out"
        );
        assert_eq!(refusal(StatusCode::FORBIDDEN, ""), "403 Forbidden");
    }

    /// T-4802: the example names a type the Endpoint serves, never the model's first class.
    #[test]
    fn the_example_is_a_type_the_endpoint_serves() {
        let list = json!({"id": "urn:ngsi-ld:EntityTypeList:1", "type": "EntityTypeList",
                          "typeList": ["Bridge"]});
        let served = Some(served_types(&list));
        let guide = || Some("AdministrativeArea".to_owned());
        assert_eq!(
            example_of(guide(), served.clone()).as_deref(),
            Some("Bridge")
        );
        assert_eq!(
            example_of(Some("Bridge".to_owned()), served).as_deref(),
            Some("Bridge")
        );
        let both = Some(vec!["AdministrativeArea".to_owned(), "Bridge".to_owned()]);
        assert_eq!(
            example_of(guide(), both).as_deref(),
            Some("AdministrativeArea")
        );
        assert_eq!(
            example_of(guide(), None).as_deref(),
            Some("AdministrativeArea")
        );
        assert_eq!(
            example_of(guide(), Some(Vec::new())),
            None,
            "it serves no type"
        );
        assert_eq!(example_of(None, None), None);
        assert!(served_types(&json!({"typeList": [1, ""]})).is_empty());
        assert!(served_types(&json!([])).is_empty());

        let mut record = json!({"dct:title": "Mosty"});
        with_types(
            &mut record,
            Some("Bridge".into()),
            Some(vec!["Bridge".into()]),
        );
        assert_eq!(record["jc:exampleType"], "Bridge");
        assert_eq!(record["jc:servedTypes"], json!(["Bridge"]));
        let mut bare = json!({});
        with_types(&mut bare, None, Some(Vec::new()));
        assert_eq!(bare, json!({}), "nothing known, nothing added");
        let mut list = json!([]);
        with_types(&mut list, Some("Bridge".into()), None);
        assert_eq!(list, json!([]));
    }

    /// T-4132: the guide's example names the type; a guide without one names none.
    #[test]
    fn the_guides_example_names_the_type() {
        let guide = json!({
            "status": 426,
            "example": { "metadata": { "x-op": "x-subscribe" },
                         "body": { "type": "Subscription", "entities": [{ "type": "Vehicle" }] } }
        });
        assert_eq!(guide_type(&guide).as_deref(), Some("Vehicle"));
        for other in [
            json!({}),
            json!({ "example": { "body": { "entities": [] } } }),
            json!({ "example": { "body": { "entities": [{ "type": "" }] } } }),
        ] {
            assert_eq!(guide_type(&other), None, "{other}");
        }
    }
    /// A loopback gateway answering `(path, status, body)` and 404 to anything else, one request
    /// per connection; the thread ends with the test binary.
    fn gateway(routes: Vec<(&'static str, u16, &'static str)>) -> String {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a loopback port");
        let base = format!(
            "http://{}/api/endpoint/e",
            listener.local_addr().expect("bound")
        );
        std::thread::spawn(move || {
            for mut stream in listener.incoming().flatten() {
                let mut head = [0u8; 2048];
                let read = stream.read(&mut head).unwrap_or(0);
                let request = String::from_utf8_lossy(head.get(..read).unwrap_or_default());
                let path = request.split_whitespace().nth(1).unwrap_or("/");
                let (status, body) = routes
                    .iter()
                    .find(|(route, _, _)| path.ends_with(route))
                    .map_or((404, ""), |(_, status, body)| (*status, *body));
                let _ = stream.write_all(
                    format!(
                        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                );
            }
        });
        base
    }

    /// T-6091: a load refusal or a gateway failure on the type reads fails the record, so the
    /// catalogue keeps the page it has; an Endpoint that answers keeps its typed example, and one
    /// that refuses the reader (401, 404) is published without one, as before.
    #[test]
    fn a_failed_type_read_fails_the_record_instead_of_dropping_the_type() {
        let types = r#"{"type":"EntityTypeList","typeList":["PointOfInterest"]}"#;
        let answered = gateway(vec![("/ws", 426, "{}"), ("/ngsi-ld/v1/types", 200, types)]);
        let (kind, served) = example_type(&answered).expect("both read");
        assert_eq!(kind.as_deref(), Some("PointOfInterest"));
        assert_eq!(served, Some(vec!["PointOfInterest".to_owned()]));

        for (status, body) in [
            (429, "too many"),
            (503, r#"{"detail":"the broker restarts"}"#),
        ] {
            let failing = gateway(vec![
                ("/ws", 426, "{}"),
                ("/ngsi-ld/v1/types", status, body),
            ]);
            let error = example_type(&failing)
                .expect_err("a failed read")
                .to_string();
            assert!(error.contains(&status.to_string()), "{error}");
            assert!(error.contains("/ngsi-ld/v1/types"), "{error}");
        }
        let refused = gateway(vec![("/ws", 503, "")]);
        assert!(
            example_type(&refused).is_err(),
            "the guide's failure counts too"
        );

        let private = gateway(vec![("/ws", 401, ""), ("/ngsi-ld/v1/types", 401, "")]);
        assert_eq!(
            example_type(&private).expect("a refusal is an answer"),
            (None, None)
        );

        let silent = std::net::TcpListener::bind("127.0.0.1:0").expect("a port");
        let address = silent.local_addr().expect("bound");
        std::thread::spawn(move || silent.incoming().for_each(drop));
        let hung_up = format!("http://{address}/api/endpoint/e");
        assert!(example_type(&hung_up).is_err(), "no answer is no answer");
    }
}
