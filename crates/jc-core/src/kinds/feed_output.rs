//! The named feed outputs an Endpoint writes in `spec.representations.feed` (Atom, RSS 2.0 and
//! JSON Feed, API/02 §8b "Feeds", EP-150, EP-151). An output is data: attribute paths per role
//! over one or more model classes, never an expression. The broker renders it (Antares
//! `antares-feed`, T-5202), the Portal writes it into the view as it stands here, and the save
//! checks its shape and, where the space has a model, every role against it.

use std::collections::{BTreeMap, BTreeSet};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_norway::Value;

use super::ical_mapping::{fits, is_name, is_path, is_plain, resolve, Accepts, Model};

/// The most outputs one Endpoint lists (EP-151).
pub const MAX_FEED_OUTPUTS: usize = 20;

/// The largest page an output may name (EP-150).
pub const MAX_FEED_LIMIT: u64 = 200;

/// The slots that say an item appeared at a time (an event's start, an article's or a notice's
/// publication): a preset fits only a type that has one of them.
const ITEM_TIMES: [&str; 5] = [
    "startDate",
    "datePublished",
    "dateCreated",
    "dateIssued",
    "dateModified",
];

/// The `feed` contract version this platform writes and the broker reads (T-5207).
pub const FEED_OUTPUT_VERSION: u64 = 1;

/// The three documents of one output: the file name after `feed/v1/{name}/`, the media type
/// and the format's name (API/02 §8b).
pub const FEED_FORMATS: [(&str, &str, &str); 3] = [
    ("atom.xml", "application/atom+xml", "Atom"),
    ("rss.xml", "application/rss+xml", "RSS 2.0"),
    ("feed.json", "application/feed+json", "JSON Feed"),
];

/// Each output's three documents, as a record, a page head and a catalogue list them:
/// `(output, path under the Endpoint, format name, media type)`.
pub fn feed_links(
    outputs: &[FeedOutput],
) -> Vec<(&FeedOutput, String, &'static str, &'static str)> {
    outputs
        .iter()
        .flat_map(|output| {
            FEED_FORMATS.iter().map(move |(file, media_type, format)| {
                (
                    output,
                    format!("feed/v1/{}/{file}", output.name),
                    *format,
                    *media_type,
                )
            })
        })
        .collect()
}

fn first_version() -> u64 {
    FEED_OUTPUT_VERSION
}

fn is_first_version(v: &u64) -> bool {
    *v == FEED_OUTPUT_VERSION
}

/// One output of `spec.representations.feed`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FeedOutput {
    /// A slug `[a-z0-9-]`, 1 to 63, unique on the Endpoint: the URL segment after `feed/v1/`.
    pub name: String,
    /// The contract version, 1 when left out.
    #[serde(default = "first_version", skip_serializing_if = "is_first_version")]
    pub version: u64,
    /// The feed's title per language (`sk`, `en`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub title: BTreeMap<String, String>,
    /// The feed's description per language.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub description: BTreeMap<String, String>,
    /// The model classes the feed lists, each with its roles.
    pub sources: Vec<FeedSource>,
    /// What narrows the view for this output; it never widens it.
    #[serde(default, skip_serializing_if = "FeedFilter::is_empty")]
    pub filter: FeedFilter,
    /// The feed's language when the request names none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lang: Option<String>,
    /// The default page size, 1 to 200 (50 when left out).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u64>,
    /// The order: `updated`, newest first, the only one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sort: Option<FeedSort>,
}

/// The order of a feed's entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum FeedSort {
    /// Newest first by the `updated` role.
    Updated,
}

/// One model class of an output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FeedSource {
    /// The NGSI-LD type.
    #[serde(rename = "type")]
    pub entity_type: String,
    /// The starting roles.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<FeedPreset>,
    /// Roles on top of the preset, or the whole mapping without one.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub mapping: BTreeMap<FeedRole, FeedPaths>,
}

/// A named starting mapping (API/02 §8b, the presets table).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum FeedPreset {
    /// Smart Data Models `Event`.
    Event,
    /// An article or a notice (schema.org `Article`).
    Article,
    /// Any type with a `name`.
    Generic,
}

/// The roles of an entry.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum FeedRole {
    /// The entry's title (required).
    Title,
    /// A short text.
    Summary,
    /// The whole text.
    Content,
    /// The page the entry is about.
    Link,
    /// When the entry last changed (required).
    Updated,
    /// When the entry first appeared.
    Published,
    /// Who wrote or organizes it.
    Author,
    /// Its categories.
    Category,
    /// An image of it.
    Image,
}

impl FeedRole {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Title => "title",
            Self::Summary => "summary",
            Self::Content => "content",
            Self::Link => "link",
            Self::Updated => "updated",
            Self::Published => "published",
            Self::Author => "author",
            Self::Category => "category",
            Self::Image => "image",
        }
    }

    /// What the role's attribute must hold; `None` takes any value the broker reads as text.
    const fn accepts(self) -> Option<Accepts> {
        match self {
            Self::Updated | Self::Published => Some(Accepts::Date),
            Self::Link | Self::Image => Some(Accepts::Uri),
            _ => None,
        }
    }
}

/// One attribute path, or a list of them: the first with a value wins.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum FeedPaths {
    /// One path.
    One(String),
    /// The first of several with a value.
    Many(Vec<String>),
}

impl FeedPaths {
    fn list(&self) -> Vec<&str> {
        match self {
            Self::One(p) => vec![p.as_str()],
            Self::Many(ps) => ps.iter().map(String::as_str).collect(),
        }
    }
}

/// The NGSI-LD query an output adds to the view's.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FeedFilter {
    /// An NGSI-LD `q`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub q: Option<String>,
    /// An NGSI-LD `scopeQ`.
    #[serde(default, rename = "scopeQ", skip_serializing_if = "Option::is_none")]
    pub scope_q: Option<String>,
    /// `georel` of a geo query.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub georel: Option<String>,
    /// `geometry` of a geo query.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub geometry: Option<String>,
    /// `coordinates` of a geo query.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coordinates: Option<String>,
    /// `geoproperty` of a geo query.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub geoproperty: Option<String>,
}

impl FeedFilter {
    fn is_empty(&self) -> bool {
        self.entries().iter().all(|(_, v)| v.is_none())
    }

    fn entries(&self) -> [(&'static str, Option<&str>); 6] {
        [
            ("q", self.q.as_deref()),
            ("scopeQ", self.scope_q.as_deref()),
            ("georel", self.georel.as_deref()),
            ("geometry", self.geometry.as_deref()),
            ("coordinates", self.coordinates.as_deref()),
            ("geoproperty", self.geoproperty.as_deref()),
        ]
    }
}

fn preset_roles(preset: FeedPreset) -> BTreeMap<FeedRole, Vec<&'static str>> {
    use FeedRole as R;
    let roles: &[(FeedRole, &[&str])] = match preset {
        FeedPreset::Event => &[
            (R::Title, &["name"]),
            (R::Updated, &["modifiedAt"]),
            (R::Published, &["createdAt"]),
            (R::Summary, &["description"]),
            (R::Link, &["url"]),
            (R::Author, &["organizer"]),
            (R::Category, &["category"]),
            (R::Image, &["image"]),
        ],
        FeedPreset::Article => &[
            (R::Title, &["headline", "name"]),
            (R::Updated, &["dateModified", "modifiedAt"]),
            (R::Published, &["datePublished", "dateCreated"]),
            (R::Summary, &["description"]),
            (R::Content, &["articleBody"]),
            (R::Link, &["url"]),
            (R::Author, &["author"]),
            (R::Category, &["category"]),
            (R::Image, &["image"]),
        ],
        FeedPreset::Generic => &[
            (R::Title, &["name"]),
            (R::Updated, &["modifiedAt"]),
            (R::Summary, &["description"]),
            (R::Link, &["url"]),
        ],
    };
    roles.iter().map(|(r, p)| (*r, p.to_vec())).collect()
}

impl FeedSource {
    /// The roles in force: the preset's, each written role replacing its own.
    fn effective(&self) -> BTreeMap<FeedRole, Vec<&str>> {
        let mut roles = self.preset.map(preset_roles).unwrap_or_default();
        for (role, paths) in &self.mapping {
            roles.insert(*role, paths.list());
        }
        roles
    }
}

/// Parentheses and double quotes of an NGSI-LD query balance outside its strings, so the broker's
/// `(filter);(request)` join keeps the filter whole.
fn balanced(text: &str) -> bool {
    let (mut depth, mut quoted) = (0usize, false);
    for c in text.chars() {
        match (c, quoted) {
            ('"', _) => quoted = !quoted,
            ('(', false) => depth += 1,
            (')', false) => match depth.checked_sub(1) {
                Some(d) => depth = d,
                None => return false,
            },
            _ => {}
        }
    }
    depth == 0 && !quoted
}

/// A slug: `[a-z0-9]` then `[a-z0-9-]`, 1 to 63.
fn is_slug(name: &str) -> bool {
    (1..=63).contains(&name.len())
        && !name.starts_with('-')
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// One source's gaps: an NGSI-LD type name, attribute paths per role, the required roles.
fn source_gaps(name: &str, source: &FeedSource, gaps: &mut Vec<String>) {
    let ty = &source.entity_type;
    if !is_name(ty) {
        gaps.push(format!(
            "feed output `{name}` names the type `{ty}`, which is not an NGSI-LD type name"
        ));
        return;
    }
    for (role, paths) in &source.mapping {
        let paths = paths.list();
        if paths.is_empty() || !paths.iter().all(|p| is_path(p)) {
            gaps.push(format!(
                "feed output `{name}`, {ty}: {} names attribute paths such as name or address.addressLocality",
                role.as_str()
            ));
        }
    }
    let roles = source.effective();
    for required in [FeedRole::Title, FeedRole::Updated] {
        if !roles.contains_key(&required) {
            gaps.push(format!(
                "feed output `{name}`, {ty}: no {} role; name a preset or map it",
                required.as_str()
            ));
        }
    }
}

impl FeedOutput {
    /// The title in `lang`, else the first, else the name: what a menu or a catalogue lists.
    pub fn label(&self, lang: &str) -> &str {
        self.title
            .get(lang)
            .or_else(|| self.title.values().next())
            .map_or(self.name.as_str(), String::as_str)
    }

    fn shape_gaps(&self) -> Vec<String> {
        let name = &self.name;
        let mut gaps = Vec::new();
        if !is_slug(name) {
            gaps.push(format!(
                "feed output `{name}`: name is a slug of a-z, 0-9 and -, 1 to 63 characters, such as kultura"
            ));
        }
        if self.version != FEED_OUTPUT_VERSION {
            gaps.push(format!(
                "feed output `{name}` is version {}, and this platform writes version {FEED_OUTPUT_VERSION}",
                self.version
            ));
        }
        for (member, texts, max) in [
            ("title", &self.title, 200),
            ("description", &self.description, 1000),
        ] {
            for (lang, text) in texts {
                if !is_two_letter(lang) {
                    gaps.push(format!("feed output `{name}`: {member} key `{lang}` is a two-letter language such as sk"));
                }
                if !is_plain(text, max) {
                    gaps.push(format!("feed output `{name}`: {member} in `{lang}` is plain text of 1 to {max} characters"));
                }
            }
        }
        if let Some(lang) = self.lang.as_deref().filter(|l| !is_two_letter(l)) {
            gaps.push(format!(
                "feed output `{name}`: lang `{lang}` is a two-letter language such as sk"
            ));
        }
        if self
            .limit
            .is_some_and(|l| !(1..=MAX_FEED_LIMIT).contains(&l))
        {
            gaps.push(format!(
                "feed output `{name}`: limit is 1 to {MAX_FEED_LIMIT}"
            ));
        }
        if self.sources.is_empty() || self.sources.len() > MAX_FEED_OUTPUTS {
            gaps.push(format!(
                "feed output `{name}`: sources lists 1 to {MAX_FEED_OUTPUTS} types"
            ));
        }
        self.filter_gaps(&mut gaps);
        for source in &self.sources {
            source_gaps(name, source, &mut gaps);
        }
        gaps
    }

    /// The filter's gaps: each query plain and balanced, a geo filter whole.
    fn filter_gaps(&self, gaps: &mut Vec<String>) {
        let name = &self.name;
        for (member, value) in self.filter.entries() {
            if let Some(value) = value {
                if !is_plain(value, 1024) || !balanced(value) {
                    gaps.push(format!(
                        "feed output `{name}`: filter.{member} is one NGSI-LD query of at most 1024 characters with balanced parentheses and quotes"
                    ));
                }
            }
        }
        let geo = [
            &self.filter.georel,
            &self.filter.geometry,
            &self.filter.coordinates,
        ];
        if geo.iter().any(|g| g.is_some()) && !geo.iter().all(|g| g.is_some()) {
            gaps.push(format!(
                "feed output `{name}`: a geo filter names georel, geometry and coordinates together"
            ));
        }
    }

    /// Every gap of the output against the space's LinkML model, one sentence each: a type the
    /// model lacks, a written path that does not resolve or does not fit its role, a required role
    /// none of whose paths the type has.
    fn model_gaps(&self, schema: &Value) -> Vec<String> {
        let model = Model { schema };
        let name = &self.name;
        let mut gaps = Vec::new();
        for source in &self.sources {
            let ty = &source.entity_type;
            if model.class(ty).is_none() {
                gaps.push(format!(
                    "feed output `{name}` lists the type {ty}, which the data model does not have"
                ));
                continue;
            }
            // Owner 2026-10-22: a feed is for items that appear over time. A preset fits a type
            // with a start or publish time; any other type names its title and time itself.
            let timed = ITEM_TIMES.iter().any(|slot| {
                resolve(&model, ty, slot).is_ok_and(|s| fits(&model, &s, Accepts::Date))
            });
            let explicit = [FeedRole::Title, FeedRole::Updated]
                .iter()
                .all(|role| source.mapping.contains_key(role));
            if source.preset.is_some() && !timed && !explicit {
                gaps.push(format!(
                    "feed output `{name}`, {ty}: the type has no start or publish time ({}), so its items do not appear over time; map title and updated explicitly or leave it out",
                    ITEM_TIMES.join(", ")
                ));
                continue;
            }
            for (role, paths) in source.effective() {
                let written = source.mapping.contains_key(&role);
                let required = matches!(role, FeedRole::Title | FeedRole::Updated);
                let resolved: Vec<(&str, Result<_, String>)> =
                    paths.iter().map(|p| (*p, resolve(&model, ty, p))).collect();
                if written {
                    for (path, slot) in &resolved {
                        match slot {
                            Err(why) => gaps.push(format!("feed output `{name}`, {ty}: {} names {why}", role.as_str())),
                            Ok(slot) if role.accepts().is_some_and(|a| !fits(&model, slot, a)) => gaps.push(format!(
                                "feed output `{name}`, {ty}: {} reads `{path}`, which holds no {}",
                                role.as_str(),
                                if role.accepts() == Some(Accepts::Date) { "date or datetime" } else { "uri" }
                            )),
                            Ok(_) => {}
                        }
                    }
                } else if required && resolved.iter().all(|(_, slot)| slot.is_err()) {
                    gaps.push(format!(
                        "feed output `{name}`, {ty}: the preset reads {} as {}, which the type does not have; map {} to an attribute it has",
                        role.as_str(),
                        paths.join(" or "),
                        role.as_str()
                    ));
                }
            }
        }
        gaps
    }
}

/// Every gap of an Endpoint's feed outputs as a list: the count, duplicate names and each output's
/// shape (EP-151), then, with a `linkml` model, each role against it. `Err` when the model does not
/// parse.
pub fn feed_gaps(outputs: &[FeedOutput], linkml: Option<&str>) -> Result<Vec<String>, String> {
    let mut gaps = Vec::new();
    if outputs.len() > MAX_FEED_OUTPUTS {
        gaps.push(format!(
            "spec.representations.feed lists {} outputs, and an Endpoint at most {MAX_FEED_OUTPUTS}",
            outputs.len()
        ));
    }
    let mut seen = BTreeSet::new();
    for output in outputs {
        if !seen.insert(output.name.as_str()) {
            gaps.push(format!(
                "feed output `{}` is listed twice; each name is one URL",
                output.name
            ));
        }
        gaps.extend(output.shape_gaps());
    }
    if !gaps.is_empty() {
        return Ok(gaps);
    }
    if let Some(text) = linkml {
        let schema: Value = serde_norway::from_str(text).map_err(|err| err.to_string())?;
        for output in outputs {
            gaps.extend(output.model_gaps(&schema));
        }
    }
    Ok(gaps)
}

/// A two-letter language code such as `sk`.
fn is_two_letter(language: &str) -> bool {
    language.len() == 2 && language.chars().all(|c| c.is_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_query_must_balance_outside_its_strings() {
        assert!(balanced(r#"(a=="x)");b>1"#));
        assert!(!balanced("a==1)"));
        assert!(!balanced("(a==1"));
        assert!(!balanced(r#"a=="x"#));
    }
}
