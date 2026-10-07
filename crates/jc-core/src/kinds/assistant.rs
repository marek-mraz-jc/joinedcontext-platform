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
/// The longest a connector's tool call may take.
pub const MAX_TIMEOUT_SECONDS: u16 = 120;

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

/// Desired specification of a [`KnowledgeSource`][crate::kinds::KnowledgeSource] (MF-51).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct KnowledgeSourceSpec {
    /// `website` (pages and their PDFs), `ckan` (a catalogue's datasets) or `catalogue` (the
    /// project's own Endpoints and the models of their spaces).
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
    /// Whether the site's `sitemap.xml` is read before links are followed; default true.
    #[serde(default = "default_true")]
    pub sitemap: bool,
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
            if !is_cron(schedule) {
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
    /// The Endpoint's `metadata.name` in this project.
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
}

/// How many model tokens the deployment may spend; `jc-agent-proxy` holds it to this.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Budget {
    /// Tokens per day across all conversations; 1 to 100,000,000.
    pub tokens_per_day: u64,
    /// Tokens one conversation may spend; at least 1 and no more than a day's.
    pub tokens_per_conversation: u64,
}

/// How the chat looks where it is placed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Theme {
    /// The accent colour, `#rrggbb`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary_color: Option<String>,
    /// The first line the chat shows, at most 500 characters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub greeting: Option<String>,
}

/// Desired specification of an [`AssistantDeployment`][crate::kinds::AssistantDeployment] (MF-52).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AssistantDeploymentSpec {
    /// The id in the widget's address, `/w/{publicId}`; a DNS label.
    pub public_id: String,
    /// `public`, `internal`, `ckan` or `iframe`.
    pub channel: Channel,
    /// What the assistant is told before every conversation; at most 8,000 characters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    /// The `KnowledgeSource`s of this project it answers from.
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
}

impl Kind for AssistantDeploymentSpec {
    const KIND: &'static str = "AssistantDeployment";
    const PLURAL: &'static str = "assistantdeployments";
    const SCOPE: Scope = Scope::Project;
    const PATH_TEMPLATE: &'static str = "projects/{project}/assistant/deployments/{name}.yaml";

    fn validate_spec(&self, meta: &ObjectMeta) -> Result<()> {
        names::validate_dns1123_label(&meta.name)?;
        self.validate()
    }
}

impl AssistantDeploymentSpec {
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
            names::validate_dns1123_label(source).map_err(|_| {
                name(
                    "sources",
                    source.clone(),
                    "a source is the name of a KnowledgeSource of this project",
                )
            })?;
        }
        for connector in &self.connectors {
            names::validate_dns1123_label(&connector.endpoint).map_err(|_| {
                name(
                    "connectors[].endpoint",
                    connector.endpoint.clone(),
                    "a connector names an Endpoint of this project",
                )
            })?;
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
        }
        languages(&self.languages)
    }
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

/// Five fields of digits and `* , - /`.
fn is_cron(text: &str) -> bool {
    let fields: Vec<&str> = text.split_whitespace().collect();
    fields.len() == 5
        && fields.iter().all(|field| {
            field
                .chars()
                .all(|c| c.is_ascii_digit() || matches!(c, '*' | ',' | '-' | '/'))
        })
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
