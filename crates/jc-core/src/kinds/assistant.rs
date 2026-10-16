//! `kind: KnowledgeSource` and `kind: AssistantDeployment`, the knowledge assistant's two
//! manifests (MF-51, MF-52, ADR-N-040, Architecture/22 §2).
//!
//! A source is what the assistant reads, a deployment is where it answers. Every rule that can
//! be judged from one manifest is judged here; the references between manifests (a source's
//! `CkanInstance`, a deployment's sources and Endpoints, an internal source on a public channel)
//! are `jcctl validate`'s, which sees the whole project.

use crate::envelope::{Kind, ObjectMeta, Scope};
use crate::error::{Error, Result};
use crate::names;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The most start URLs one source may name.
pub const MAX_START_URLS: usize = 20;
/// The most include or exclude patterns one source may name.
pub const MAX_PATTERNS: usize = 50;
/// The longest include or exclude pattern.
pub const MAX_PATTERN_LEN: usize = 256;
/// The deepest a crawl may follow links from a start URL.
pub const MAX_DEPTH: u8 = 10;
/// The most pages one source may hold.
pub const MAX_PAGES: u32 = 50_000;
/// The largest PDF a source may fetch: 200 MiB.
pub const MAX_PDF_BYTES: u64 = 200 * 1024 * 1024;
/// The most pages of one PDF a source may extract.
pub const MAX_PDF_PAGES: u32 = 2_000;
/// The longest system prompt.
pub const MAX_PROMPT_LEN: usize = 8_000;
/// The longest greeting.
pub const MAX_GREETING_LEN: usize = 500;
/// The longest name in the chat's header.
pub const MAX_TITLE_LEN: usize = 60;
/// The longest a connector's tool call may take.
pub const MAX_TIMEOUT_SECONDS: u16 = 120;
/// The most WordPress post types one `website` source may list (T-4154).
pub const MAX_WORDPRESS_TYPES: usize = 40;

/// The most CKAN organizations one `ckan` source may narrow to.
pub const MAX_ORGANIZATIONS: usize = 10;
/// The largest file an `upload` source may hold: 50 MiB.
pub const MAX_UPLOAD_BYTES: u64 = 50 * 1024 * 1024;
/// The most files an `upload` source may hold.
pub const MAX_UPLOAD_FILES: u32 = 500;
/// The model a deployment's router and answer calls use when it names none (MF-52).
pub const DEFAULT_MODEL: &str = "google/gemini-3.8-flash";
/// The longest model id.
pub const MAX_MODEL_ID_LEN: usize = 100;
/// The most suggested questions per language.
pub const MAX_SUGGESTIONS: usize = 6;
/// The longest suggested question.
pub const MAX_SUGGESTION_LEN: usize = 120;
/// The longest disclaimer.
pub const MAX_DISCLAIMER_LEN: usize = 400;
/// The most other projects a deployment's connectors may reach.
pub const MAX_SCOPE_PROJECTS: usize = 10;

/// Where a source's knowledge comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum SourceType {
    /// Pages and the PDFs they link, crawled from `startUrls`.
    Website,
    /// The datasets of a `CkanInstance` of the project.
    Ckan,
    /// The project's own NGSI-LD catalogue: one page per Endpoint of its context spaces, with the
    /// types and attributes of the space's model (AG-116).
    Catalogue,
    /// The platform's User Guide as jc-assistant ships it: one passage per section, its Portal
    /// paths pointing into the source's own project (AG-118).
    Guide,
    /// Files an administrator uploads: PDF, DOCX, XLSX, CSV, Markdown or plain text (MF-51).
    Upload,
}

/// Who may read what a source holds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum Visibility {
    /// Anyone, through any channel.
    Public,
    /// People of the organization, through the internal channel only.
    #[default]
    Internal,
}

/// Whether linked PDFs are read.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum PdfPolicyKind {
    /// PDFs are fetched and their text is read.
    #[default]
    Include,
    /// PDFs are skipped.
    Exclude,
}

/// What the crawler does with PDFs, and how large one may be.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct PdfPolicy {
    /// `include` (default) or `exclude`.
    #[serde(default)]
    pub policy: PdfPolicyKind,
    /// The largest PDF read, in bytes; at most 200 MiB, default 50 MiB.
    #[serde(default = "default_pdf_bytes")]
    pub max_bytes: u64,
    /// The most pages of one PDF read; at most 2,000, default 500.
    #[serde(default = "default_pdf_pages")]
    pub max_pages: u32,
}

impl Default for PdfPolicy {
    fn default() -> Self {
        Self {
            policy: PdfPolicyKind::default(),
            max_bytes: default_pdf_bytes(),
            max_pages: default_pdf_pages(),
        }
    }
}

fn default_pdf_bytes() -> u64 {
    50 * 1024 * 1024
}
fn default_pdf_pages() -> u32 {
    500
}
fn default_true() -> bool {
    true
}
fn default_depth() -> u8 {
    3
}
fn default_pages() -> u32 {
    1_000
}
fn default_timeout() -> u16 {
    20
}
fn default_upload_bytes() -> u64 {
    20 * 1024 * 1024
}
fn default_upload_files() -> u32 {
    100
}
fn default_per_hour() -> u32 {
    60
}
fn default_per_day() -> u32 {
    200
}
fn default_per_month() -> u32 {
    300
}
fn default_streams() -> u32 {
    2
}
fn default_cost_per_day() -> f64 {
    3.0
}
fn default_output_tokens() -> u32 {
    700
}
fn default_messages() -> u32 {
    30
}
fn default_question_chars() -> u32 {
    1_000
}

/// How large and how many the files of an `upload` source may be.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct UploadLimits {
    /// The largest file, in bytes; at most 50 MiB, default 20 MiB.
    #[serde(default = "default_upload_bytes")]
    pub max_bytes: u64,
    /// The most files; 1 to 500, default 100.
    #[serde(default = "default_upload_files")]
    pub max_files: u32,
}

impl Default for UploadLimits {
    fn default() -> Self {
        Self {
            max_bytes: default_upload_bytes(),
            max_files: default_upload_files(),
        }
    }
}

/// Desired specification of a [`KnowledgeSource`][crate::kinds::KnowledgeSource] (MF-51).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct KnowledgeSourceSpec {
    /// `website` (pages and their PDFs), `ckan` (a catalogue's datasets), `catalogue` (the
    /// project's own Endpoints and the models of their spaces), `guide` (the platform's User
    /// Guide, AG-118) or `upload` (files an administrator uploads).
    pub source: SourceType,
    /// Where a website crawl starts: one to twenty absolute `https` URLs. Empty for `ckan`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub start_urls: Vec<String>,
    /// The `CkanInstance` of this project a `ckan` source reads. Absent for `website`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ckan_instance_ref: Option<String>,
    /// The context spaces of this project a `catalogue` source reads; empty reads every one.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub context_spaces: Vec<String>,
    /// The CKAN organizations a `ckan` source indexes, by name; empty indexes every one.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub organizations: Vec<String>,
    /// How large and how many an `upload` source's files may be; the defaults when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upload: Option<UploadLimits>,
    /// Whether the site's `sitemap.xml` is read before links are followed; default true.
    #[serde(default = "default_true")]
    pub sitemap: bool,
    /// WordPress REST bases (`pages`, `newses`, ...) whose items' links are read beside the
    /// sitemap, from the start host's `/wp-json/wp/v2/{type}`, newest change first (T-4154).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub wordpress_types: Vec<String>,
    /// Path patterns a page must match to be read, each starting with `/`; empty reads all.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub include: Vec<String>,
    /// Path patterns whose pages are skipped, each starting with `/`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<String>,
    /// How many links deep a crawl follows from a start URL; 1 to 10, default 3.
    #[serde(default = "default_depth")]
    pub max_depth: u8,
    /// The most pages the source holds; 1 to 50,000, default 1,000.
    #[serde(default = "default_pages")]
    pub max_pages: u32,
    /// What happens to linked PDFs.
    #[serde(default)]
    pub pdf: PdfPolicy,
    /// Whether a document linked from another host is read; default false, so a source never
    /// grows into somebody else's site.
    #[serde(default)]
    pub off_domain_documents: bool,
    /// When the source is read again, as five-field cron (`minute hour day month weekday`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule: Option<String>,
    /// The languages its text is in, as ISO 639-1 codes; they choose the search stemmer.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub languages: Vec<String>,
    /// `public` (any channel) or `internal` (the internal channel only); default `internal`.
    #[serde(default)]
    pub visibility: Visibility,
}

impl Kind for KnowledgeSourceSpec {
    const KIND: &'static str = "KnowledgeSource";
    const PLURAL: &'static str = "knowledgesources";
    const SCOPE: Scope = Scope::Project;
    const PATH_TEMPLATE: &'static str = "projects/{project}/assistant/sources/{name}.yaml";

    fn validate_spec(&self, meta: &ObjectMeta) -> Result<()> {
        names::validate_dns1123_label(&meta.name)?;
        self.validate()
    }
}

impl KnowledgeSourceSpec {
    /// The upload limits in force: the manifest's, or the defaults.
    pub fn upload_limits(&self) -> UploadLimits {
        self.upload.clone().unwrap_or_default()
    }

    /// Every rule of MF-51 that one manifest can be judged by.
    pub fn validate(&self) -> Result<()> {
        match self.source {
            SourceType::Website => {
                if self.start_urls.is_empty() || self.start_urls.len() > MAX_START_URLS {
                    return Err(name(
                        "startUrls",
                        self.start_urls.len().to_string(),
                        "a website source names one to twenty start URLs",
                    ));
                }
                if self.ckan_instance_ref.is_some() {
                    return Err(name(
                        "ckanInstanceRef",
                        String::new(),
                        "a website source names no CkanInstance; use source: ckan",
                    ));
                }
                for url in &self.start_urls {
                    https_url(url).ok_or_else(|| {
                        name(
                            "startUrls",
                            url.clone(),
                            "a start URL is an absolute https:// address with a host",
                        )
                    })?;
                }
            }
            SourceType::Ckan => {
                let instance = self.ckan_instance_ref.as_deref().unwrap_or_default();
                if names::validate_dns1123_label(instance).is_err() {
                    return Err(name(
                        "ckanInstanceRef",
                        instance.to_owned(),
                        "a ckan source names the CkanInstance of this project it reads",
                    ));
                }
                if !self.start_urls.is_empty() {
                    return Err(name(
                        "startUrls",
                        self.start_urls.join(", "),
                        "a ckan source reads its CkanInstance and names no start URLs",
                    ));
                }
            }
            SourceType::Catalogue => {
                if !self.start_urls.is_empty() || self.ckan_instance_ref.is_some() {
                    return Err(name(
                        "source",
                        "catalogue".to_owned(),
                        "a catalogue source reads the project's own Endpoints and names no start URLs and no CkanInstance",
                    ));
                }
                for space in &self.context_spaces {
                    names::validate_space_name(space).map_err(|_| {
                        name(
                            "contextSpaces",
                            space.clone(),
                            "a context space of this project, by its name",
                        )
                    })?;
                }
            }
            SourceType::Guide => {
                if !self.start_urls.is_empty()
                    || self.ckan_instance_ref.is_some()
                    || !self.context_spaces.is_empty()
                {
                    return Err(name(
                        "source",
                        "guide".to_owned(),
                        "a guide source reads the User Guide jc-assistant ships and names no start URLs, no CkanInstance and no context spaces",
                    ));
                }
            }
            SourceType::Upload => {
                if !self.start_urls.is_empty()
                    || self.ckan_instance_ref.is_some()
                    || !self.context_spaces.is_empty()
                {
                    return Err(name(
                        "source",
                        "upload".to_owned(),
                        "an upload source holds the files an administrator uploads and names no start URLs, no CkanInstance and no context spaces",
                    ));
                }
                if self.schedule.is_some() {
                    return Err(name(
                        "schedule",
                        String::new(),
                        "an upload source has nothing to fetch on a schedule; its files are read as they are uploaded",
                    ));
                }
            }
        }
        if self.source != SourceType::Ckan && !self.organizations.is_empty() {
            return Err(name(
                "organizations",
                self.organizations.join(", "),
                "only a ckan source names CKAN organizations",
            ));
        }
        if self.organizations.len() > MAX_ORGANIZATIONS {
            return Err(name(
                "organizations",
                self.organizations.len().to_string(),
                "a ckan source names at most 10 organizations",
            ));
        }
        for organization in &self.organizations {
            if !is_ckan_name(organization) {
                return Err(name(
                    "organizations",
                    organization.clone(),
                    "a CKAN organization name: 2 to 100 lowercase letters, digits, - and _",
                ));
            }
        }
        if let Some(upload) = &self.upload {
            if self.source != SourceType::Upload {
                return Err(name(
                    "upload",
                    String::new(),
                    "only an upload source sets upload limits",
                ));
            }
            if !(1..=MAX_UPLOAD_BYTES).contains(&upload.max_bytes) {
                return Err(name(
                    "upload.maxBytes",
                    upload.max_bytes.to_string(),
                    "upload.maxBytes is 1 byte to 50 MiB (52428800)",
                ));
            }
            if !(1..=MAX_UPLOAD_FILES).contains(&upload.max_files) {
                return Err(name(
                    "upload.maxFiles",
                    upload.max_files.to_string(),
                    "upload.maxFiles is 1 to 500",
                ));
            }
        }
        if self.source != SourceType::Website && !self.wordpress_types.is_empty() {
            return Err(name(
                "wordpressTypes",
                self.wordpress_types.join(", "),
                "only a website source lists WordPress types",
            ));
        }
        if self.wordpress_types.len() > MAX_WORDPRESS_TYPES {
            return Err(name(
                "wordpressTypes",
                self.wordpress_types.len().to_string(),
                "at most 40 WordPress types",
            ));
        }
        for base in &self.wordpress_types {
            if base.is_empty()
                || base.len() > 64
                || !base
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
            {
                return Err(name(
                    "wordpressTypes",
                    base.clone(),
                    "a WordPress REST base such as pages or newses: 1 to 64 of a-z, 0-9, _ and -",
                ));
            }
        }
        if self.source != SourceType::Catalogue && !self.context_spaces.is_empty() {
            return Err(name(
                "contextSpaces",
                self.context_spaces.join(", "),
                "only a catalogue source names context spaces",
            ));
        }
        for (field, patterns) in [("include", &self.include), ("exclude", &self.exclude)] {
            if patterns.len() > MAX_PATTERNS {
                return Err(name(
                    field,
                    patterns.len().to_string(),
                    "at most 50 patterns",
                ));
            }
            for pattern in patterns {
                if !pattern.starts_with('/')
                    || pattern.len() > MAX_PATTERN_LEN
                    || pattern.chars().any(char::is_whitespace)
                {
                    return Err(name(
                        field,
                        pattern.clone(),
                        "a pattern is a path starting with `/`, at most 256 characters, no spaces",
                    ));
                }
            }
        }
        if !(1..=MAX_DEPTH).contains(&self.max_depth) {
            return Err(name(
                "maxDepth",
                self.max_depth.to_string(),
                "maxDepth is 1 to 10",
            ));
        }
        if !(1..=MAX_PAGES).contains(&self.max_pages) {
            return Err(name(
                "maxPages",
                self.max_pages.to_string(),
                "maxPages is 1 to 50000",
            ));
        }
        if !(1..=MAX_PDF_BYTES).contains(&self.pdf.max_bytes) {
            return Err(name(
                "pdf.maxBytes",
                self.pdf.max_bytes.to_string(),
                "pdf.maxBytes is 1 byte to 200 MiB (209715200)",
            ));
        }
        if !(1..=MAX_PDF_PAGES).contains(&self.pdf.max_pages) {
            return Err(name(
                "pdf.maxPages",
                self.pdf.max_pages.to_string(),
                "pdf.maxPages is 1 to 2000",
            ));
        }
        if let Some(schedule) = &self.schedule {
            if !crate::cron::is_valid(schedule) {
                return Err(name(
                    "schedule",
                    schedule.clone(),
                    "schedule is five-field cron: minute hour day month weekday",
                ));
            }
        }
        languages(&self.languages)
    }
}

/// Where a deployment answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum Channel {
    /// Anyone, on the allowed origins.
    Public,
    /// Signed-in people of the organization, in the Portal.
    Internal,
    /// Visitors of the CKAN catalogue.
    Ckan,
    /// An iframe on the allowed origins.
    Iframe,
}

impl Channel {
    /// Whether the channel answers people nobody signed in (MF-52).
    pub fn is_anonymous(self) -> bool {
        !matches!(self, Channel::Internal)
    }
}

/// One Endpoint of the project whose MCP tools the assistant may call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Connector {
    /// The Endpoint's `metadata.name` in this project, or `{project}/{name}` for a public
    /// Endpoint of a project in `scope.projects`.
    pub endpoint: String,
    /// The tools of its MCP surface the assistant may call; at least one.
    pub tools: Vec<String>,
    /// How long one tool call may take, in seconds; 1 to 120, default 20.
    #[serde(default = "default_timeout")]
    pub timeout_seconds: u16,
}

/// How often the deployment answers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RateLimit {
    /// Questions per minute across everyone; 1 to 600.
    pub requests_per_minute: u32,
    /// Questions per minute from one client; 1 to 120.
    pub per_client_per_minute: u32,
    /// Questions per hour from one client; 1 to 7,200, default 60.
    #[serde(default = "default_per_hour")]
    pub per_client_per_hour: u32,
    /// Questions per day from one client; 1 to 100,000, default 200.
    #[serde(default = "default_per_day")]
    pub per_client_per_day: u32,
    /// Questions per UTC calendar month from one client; 1 to 100,000, default 300 (T-4286).
    #[serde(default = "default_per_month")]
    pub per_client_per_month: u32,
    /// Answers one client may have streaming at once; 1 to 10, default 2.
    #[serde(default = "default_streams")]
    pub streams_per_client: u32,
}

/// How many model tokens the deployment may spend; `jc-agent-proxy` holds it to this.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Budget {
    /// Tokens per day across all conversations; 1 to 100,000,000.
    pub tokens_per_day: u64,
    /// Tokens one conversation may spend; at least 1 and no more than a day's.
    pub tokens_per_conversation: u64,
    /// What the deployment's model calls may cost per day, in US dollars; 0.01 to 1,000,
    /// default 3. Past it an answer lists the matching sources' links only (AG-107).
    #[serde(default = "default_cost_per_day")]
    pub cost_per_day_usd: f64,
    /// The most tokens one answer may have; 100 to 4,000, default 700.
    #[serde(default = "default_output_tokens")]
    pub output_tokens_per_answer: u32,
    /// The most questions one conversation may ask; 1 to 200, default 30.
    #[serde(default = "default_messages")]
    pub messages_per_conversation: u32,
    /// The longest question, in characters; 100 to 4,000, default 1,000.
    #[serde(default = "default_question_chars")]
    pub question_chars: u32,
}

/// The models a deployment asks `jc-agent-proxy` for; each [`DEFAULT_MODEL`] when absent.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ModelChoice {
    /// The model that picks the tools and sources of a question, `vendor/name`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub router: Option<String>,
    /// The model that writes the answer, `vendor/name`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer: Option<String>,
}

/// What one answer may use (T-4281).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Routes {
    /// The sources and the connectors, as the router picks.
    #[default]
    Both,
    /// The sources alone: no connector is offered and no tool called.
    Rag,
    /// The connectors alone: no search round.
    Data,
}

/// How the deployment answers.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AnswerSpec {
    /// What one answer may use; default `both`.
    #[serde(default)]
    pub routes: Routes,
}

/// What a deployment reaches beyond its own project.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AssistantScope {
    /// Other projects whose `audience: public` Endpoints its connectors, and whose
    /// `KnowledgeSource`s its sources, may name; at most 10, never its own and never `*`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub projects: Vec<String>,
}

/// How the chat looks where it is placed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Theme {
    /// The body's name in the chat's header, beside its monogram; at most 60 characters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The accent colour, `#rrggbb`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary_color: Option<String>,
    /// The first line the chat shows, at most 500 characters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub greeting: Option<String>,
}

/// Desired specification of an [`AssistantDeployment`][crate::kinds::AssistantDeployment] (MF-52).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AssistantDeploymentSpec {
    /// The id in the widget's address, `/w/{publicId}`; a DNS label.
    pub public_id: String,
    /// `public`, `internal`, `ckan` or `iframe`.
    pub channel: Channel,
    /// What the assistant is told before every conversation; at most 8,000 characters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    /// The `KnowledgeSource`s it answers from: one of this project by its name, one of a project
    /// in `scope.projects` as `{project}/{source}` (T-4281).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<String>,
    /// The Endpoints of this project whose MCP tools it may call.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub connectors: Vec<Connector>,
    /// The pages it may be placed on, each `https://host[:port]`; no wildcard, no path.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_origins: Vec<String>,
    /// How often it answers; required on every channel but `internal`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimit>,
    /// How much it may spend; required on every channel but `internal`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<Budget>,
    /// How the chat looks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theme: Option<Theme>,
    /// The languages it answers in, as ISO 639-1 codes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub languages: Vec<String>,
    /// Whether it may filter a large tool result with a script in `jc-functions`; default false.
    #[serde(default)]
    pub sandbox: bool,
    /// The router and answer models; the service's default when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelChoice>,
    /// Questions the chat offers before the first one, per language of `languages`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub suggestions: BTreeMap<String, Vec<String>>,
    /// The line under the chat that says answers are machine-written, per language.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub disclaimer: BTreeMap<String, String>,
    /// Whether it answers at all; `false` makes every chat request answer `503`. Default true.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// What it reaches beyond its own project.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<AssistantScope>,
    /// How it answers: what one answer may use.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer: Option<AnswerSpec>,
    /// Whether the chat shows the helpful/unhelpful buttons under an answer; default true.
    #[serde(default = "default_true")]
    pub feedback: bool,
}

impl Kind for AssistantDeploymentSpec {
    const KIND: &'static str = "AssistantDeployment";
    const PLURAL: &'static str = "assistantdeployments";
    const SCOPE: Scope = Scope::Project;
    const PATH_TEMPLATE: &'static str = "projects/{project}/assistant/deployments/{name}.yaml";

    fn validate_spec(&self, meta: &ObjectMeta) -> Result<()> {
        names::validate_dns1123_label(&meta.name)?;
        self.validate()?;
        // A deployment's own project is reached without being named; naming it is a mistake.
        let own = meta.namespace.as_deref().unwrap_or_default();
        match self.scope_projects().iter().find(|project| *project == own) {
            Some(project) => Err(name(
                "scope.projects",
                project.clone(),
                "scope.projects names other projects; the deployment's own is reached already",
            )),
            None => Ok(()),
        }
    }
}

impl AssistantDeploymentSpec {
    /// The model id the router calls use.
    pub fn router_model(&self) -> &str {
        self.model
            .as_ref()
            .and_then(|model| model.router.as_deref())
            .unwrap_or(DEFAULT_MODEL)
    }

    /// The model id the answer call uses.
    pub fn answer_model(&self) -> &str {
        self.model
            .as_ref()
            .and_then(|model| model.answer.as_deref())
            .unwrap_or(DEFAULT_MODEL)
    }

    /// The other projects its connectors may reach; empty when it names none.
    ///
    /// A connector names one of their Endpoints as `{project}/{endpoint}` ([`connector_target`]).
    pub fn scope_projects(&self) -> &[String] {
        self.scope
            .as_ref()
            .map_or(&[], |scope| scope.projects.as_slice())
    }

    /// What one answer may use; `both` when absent.
    pub fn routes(&self) -> Routes {
        self.answer.as_ref().map(|a| a.routes).unwrap_or_default()
    }

    /// Every rule of MF-52 that one manifest can be judged by.
    pub fn validate(&self) -> Result<()> {
        names::validate_dns1123_label(&self.public_id).map_err(|_| {
            name(
                "publicId",
                self.public_id.clone(),
                "publicId is a DNS label: lowercase letters, digits and dashes",
            )
        })?;
        if self.sources.is_empty() && self.connectors.is_empty() {
            return Err(name(
                "sources",
                String::new(),
                "a deployment answers from at least one source or connector",
            ));
        }
        for source in &self.sources {
            let (project, label) = connector_target(source);
            names::validate_dns1123_label(label).map_err(|_| {
                name(
                    "sources",
                    source.clone(),
                    "a source names a KnowledgeSource of this project, or one of a project in scope.projects as {project}/{source}",
                )
            })?;
            if project.is_some_and(|project| !self.scope_projects().iter().any(|p| p == project)) {
                return Err(name(
                    "sources",
                    source.clone(),
                    "a source names another project's KnowledgeSource only when scope.projects lists that project",
                ));
            }
        }
        match self.routes() {
            Routes::Rag if self.sources.is_empty() => {
                return Err(name(
                    "answer.routes",
                    "rag".into(),
                    "routes rag answers from the sources; name at least one",
                ))
            }
            Routes::Data if self.connectors.is_empty() => {
                return Err(name(
                    "answer.routes",
                    "data".into(),
                    "routes data answers from the connectors; name at least one",
                ))
            }
            _ => {}
        }
        for connector in &self.connectors {
            let (project, endpoint) = connector_target(&connector.endpoint);
            names::validate_dns1123_label(endpoint).map_err(|_| {
                name(
                    "connectors[].endpoint",
                    connector.endpoint.clone(),
                    "a connector names an Endpoint of this project, or one of a project in scope.projects as {project}/{endpoint}",
                )
            })?;
            if project.is_some_and(|project| !self.scope_projects().iter().any(|p| p == project)) {
                return Err(name(
                    "connectors[].endpoint",
                    connector.endpoint.clone(),
                    "a connector names another project's Endpoint only when scope.projects lists that project",
                ));
            }
            if connector.tools.is_empty()
                || connector.tools.iter().any(|tool| tool.trim().is_empty())
            {
                return Err(name(
                    "connectors[].tools",
                    connector.endpoint.clone(),
                    "a connector lists the tools it may call, at least one",
                ));
            }
            if !(1..=MAX_TIMEOUT_SECONDS).contains(&connector.timeout_seconds) {
                return Err(name(
                    "connectors[].timeoutSeconds",
                    connector.timeout_seconds.to_string(),
                    "timeoutSeconds is 1 to 120",
                ));
            }
        }
        for origin in &self.allowed_origins {
            if !is_origin(origin) {
                return Err(name(
                    "allowedOrigins",
                    origin.clone(),
                    "an origin is https://host or https://host:port, with no path and no wildcard",
                ));
            }
        }
        if self.channel.is_anonymous() {
            if self.allowed_origins.is_empty() {
                return Err(name(
                    "allowedOrigins",
                    String::new(),
                    "a public, ckan or iframe deployment lists the origins it may be placed on",
                ));
            }
            if self.rate_limit.is_none() {
                return Err(name(
                    "rateLimit",
                    String::new(),
                    "a public, ckan or iframe deployment carries a rateLimit",
                ));
            }
            if self.budget.is_none() {
                return Err(name(
                    "budget",
                    String::new(),
                    "a public, ckan or iframe deployment carries a budget",
                ));
            }
        }
        if let Some(limit) = &self.rate_limit {
            if !(1..=600).contains(&limit.requests_per_minute) {
                return Err(name(
                    "rateLimit.requestsPerMinute",
                    limit.requests_per_minute.to_string(),
                    "requestsPerMinute is 1 to 600",
                ));
            }
            if !(1..=120).contains(&limit.per_client_per_minute)
                || limit.per_client_per_minute > limit.requests_per_minute
            {
                return Err(name(
                    "rateLimit.perClientPerMinute",
                    limit.per_client_per_minute.to_string(),
                    "perClientPerMinute is 1 to 120 and no more than requestsPerMinute",
                ));
            }
            for (field, value, max, reason) in [
                (
                    "rateLimit.perClientPerHour",
                    limit.per_client_per_hour,
                    7_200,
                    "perClientPerHour is 1 to 7200",
                ),
                (
                    "rateLimit.perClientPerDay",
                    limit.per_client_per_day,
                    100_000,
                    "perClientPerDay is 1 to 100000",
                ),
                (
                    "rateLimit.perClientPerMonth",
                    limit.per_client_per_month,
                    100_000,
                    "perClientPerMonth is 1 to 100000",
                ),
                (
                    "rateLimit.streamsPerClient",
                    limit.streams_per_client,
                    10,
                    "streamsPerClient is 1 to 10",
                ),
            ] {
                if !(1..=max).contains(&value) {
                    return Err(name(field, value.to_string(), reason));
                }
            }
        }
        if let Some(budget) = &self.budget {
            if !(1..=100_000_000).contains(&budget.tokens_per_day) {
                return Err(name(
                    "budget.tokensPerDay",
                    budget.tokens_per_day.to_string(),
                    "tokensPerDay is 1 to 100000000",
                ));
            }
            if budget.tokens_per_conversation == 0
                || budget.tokens_per_conversation > budget.tokens_per_day
            {
                return Err(name(
                    "budget.tokensPerConversation",
                    budget.tokens_per_conversation.to_string(),
                    "tokensPerConversation is at least 1 and no more than tokensPerDay",
                ));
            }
            if !(0.01..=1_000.0).contains(&budget.cost_per_day_usd) {
                return Err(name(
                    "budget.costPerDayUsd",
                    budget.cost_per_day_usd.to_string(),
                    "costPerDayUsd is 0.01 to 1000",
                ));
            }
            for (field, value, range, reason) in [
                (
                    "budget.outputTokensPerAnswer",
                    budget.output_tokens_per_answer,
                    100..=4_000,
                    "outputTokensPerAnswer is 100 to 4000",
                ),
                (
                    "budget.messagesPerConversation",
                    budget.messages_per_conversation,
                    1..=200,
                    "messagesPerConversation is 1 to 200",
                ),
                (
                    "budget.questionChars",
                    budget.question_chars,
                    100..=4_000,
                    "questionChars is 100 to 4000",
                ),
            ] {
                if !range.contains(&value) {
                    return Err(name(field, value.to_string(), reason));
                }
            }
        }
        if self
            .system_prompt
            .as_deref()
            .is_some_and(|prompt| prompt.chars().count() > MAX_PROMPT_LEN)
        {
            return Err(name(
                "systemPrompt",
                String::new(),
                "systemPrompt is at most 8000 characters",
            ));
        }
        if let Some(model) = &self.model {
            for (field, id) in [
                ("model.router", &model.router),
                ("model.answer", &model.answer),
            ] {
                if let Some(id) = id {
                    if !is_model_id(id) {
                        return Err(name(
                            field,
                            id.clone(),
                            "a model id is vendor/name, at most 100 characters of a-z 0-9 . _ : / -",
                        ));
                    }
                }
            }
        }
        for (language, questions) in &self.suggestions {
            self.known_language("suggestions", language)?;
            if questions.len() > MAX_SUGGESTIONS {
                return Err(name(
                    "suggestions",
                    language.clone(),
                    "at most 6 suggested questions per language",
                ));
            }
            for question in questions {
                let length = question.chars().count();
                if question.trim().is_empty() || length > MAX_SUGGESTION_LEN {
                    return Err(name(
                        "suggestions",
                        language.clone(),
                        "a suggested question is 1 to 120 characters",
                    ));
                }
            }
        }
        for (language, text) in &self.disclaimer {
            self.known_language("disclaimer", language)?;
            if text.trim().is_empty() || text.chars().count() > MAX_DISCLAIMER_LEN {
                return Err(name(
                    "disclaimer",
                    language.clone(),
                    "a disclaimer is 1 to 400 characters",
                ));
            }
        }
        // A credential pasted into text every visitor reads, or every model call carries, would
        // leak it; the refusal names the field and never repeats the value.
        let texts = self
            .system_prompt
            .iter()
            .map(|prompt| ("systemPrompt", prompt.as_str()))
            .chain(
                self.suggestions
                    .values()
                    .flatten()
                    .map(|q| ("suggestions", q.as_str())),
            )
            .chain(
                self.disclaimer
                    .values()
                    .map(|text| ("disclaimer", text.as_str())),
            );
        for (field, text) in texts {
            if text.split_whitespace().any(names::looks_like_a_credential) {
                return Err(name(
                    field,
                    String::new(),
                    "carries the shape of a credential; a key or token never goes into the text",
                ));
            }
        }
        let projects = self.scope_projects();
        if projects.len() > MAX_SCOPE_PROJECTS {
            return Err(name(
                "scope.projects",
                projects.len().to_string(),
                "scope.projects names at most 10 projects",
            ));
        }
        for (i, project) in projects.iter().enumerate() {
            if project == "*" {
                return Err(name(
                    "scope.projects",
                    project.clone(),
                    "scope.projects names each project; `*` is never allowed",
                ));
            }
            names::validate_dns1123_label(project).map_err(|_| {
                name(
                    "scope.projects",
                    project.clone(),
                    "a project is named by its DNS label",
                )
            })?;
            if projects[..i].contains(project) {
                return Err(name(
                    "scope.projects",
                    project.clone(),
                    "each project is named once",
                ));
            }
        }
        if let Some(theme) = &self.theme {
            if let Some(colour) = &theme.primary_color {
                let hex = colour.strip_prefix('#').unwrap_or_default();
                if hex.len() != 6 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
                    return Err(name(
                        "theme.primaryColor",
                        colour.clone(),
                        "primaryColor is #rrggbb",
                    ));
                }
            }
            if theme
                .greeting
                .as_deref()
                .is_some_and(|greeting| greeting.chars().count() > MAX_GREETING_LEN)
            {
                return Err(name(
                    "theme.greeting",
                    String::new(),
                    "greeting is at most 500 characters",
                ));
            }
            if theme.title.as_deref().is_some_and(|title| {
                title.trim().is_empty() || title.chars().count() > MAX_TITLE_LEN
            }) {
                return Err(name(
                    "theme.title",
                    String::new(),
                    "title is 1 to 60 characters",
                ));
            }
        }
        languages(&self.languages)
    }
}

impl AssistantDeploymentSpec {
    /// A per-language map's key must be one of `languages`.
    fn known_language(&self, field: &'static str, language: &str) -> Result<()> {
        if self.languages.iter().any(|known| known == language) {
            Ok(())
        } else {
            Err(name(
                field,
                language.to_owned(),
                "each language here is one the deployment lists in languages",
            ))
        }
    }
}

/// `vendor/name`: at most 100 characters of `a-z0-9._:/-`, both parts present.
/// The project and Endpoint a connector names: `{project}/{endpoint}` for an Endpoint of a
/// project in `scope.projects`, the deployment's own project (`None`) otherwise (MF-52, T-4177).
/// Also how a source names another project's `KnowledgeSource`, `{project}/{source}` (T-4281).
pub fn connector_target(endpoint: &str) -> (Option<&str>, &str) {
    match endpoint.split_once('/') {
        Some((project, name)) => (Some(project), name),
        None => (None, endpoint),
    }
}

fn is_model_id(id: &str) -> bool {
    id.len() <= MAX_MODEL_ID_LEN
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "._:/-".contains(c))
        && id
            .split_once('/')
            .is_some_and(|(vendor, model)| !vendor.is_empty() && !model.is_empty())
}

/// A CKAN organization name: 2 to 100 characters of `a-z0-9-_`.
fn is_ckan_name(value: &str) -> bool {
    (2..=100).contains(&value.len())
        && value
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

fn name(field: &'static str, value: String, reason: &'static str) -> Error {
    Error::Name {
        field,
        value,
        reason,
    }
}

/// `https://host[:port][/path]` with a host of letters, digits, dots and dashes; the host.
fn https_url(url: &str) -> Option<&str> {
    let rest = url.strip_prefix("https://")?;
    let authority = rest.split(['/', '?', '#']).next()?;
    let host = host_and_port(authority)?;
    (!rest[authority.len()..].contains(char::is_whitespace)).then_some(host)
}

/// `https://host[:port]` and nothing else: no path, no query, no wildcard, no user.
fn is_origin(origin: &str) -> bool {
    origin
        .strip_prefix("https://")
        .is_some_and(|authority| host_and_port(authority).is_some())
}

/// The host of `host[:port]` when both are well formed.
fn host_and_port(authority: &str) -> Option<&str> {
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) => (host, Some(port)),
        None => (authority, None),
    };
    if let Some(port) = port {
        port.parse::<u16>().ok().filter(|port| *port > 0)?;
    }
    let labels_ok = !host.is_empty()
        && host.len() <= 253
        && host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
                && !label.starts_with('-')
                && !label.ends_with('-')
        });
    labels_ok.then_some(host)
}

/// ISO 639-1: two lowercase letters each.
fn languages(codes: &[String]) -> Result<()> {
    for code in codes {
        if code.len() != 2 || !code.chars().all(|c| c.is_ascii_lowercase()) {
            return Err(name(
                "languages",
                code.clone(),
                "a language is a two-letter ISO 639-1 code such as sk, cs, fi or en",
            ));
        }
    }
    Ok(())
}
