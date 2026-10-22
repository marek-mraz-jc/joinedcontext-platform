//! The iCalendar mapping an Endpoint writes for its `ical` representation (EP-125, API/02 §8b):
//! which NGSI-LD attribute each RFC 5545 field comes from, through a fixed allow-list of remaps.
//! A mapping is data: attribute paths and remap keys, never an expression or a template. The
//! broker renders it (T-4901); `jcctl validate`, the Portal's propose and the Endpoint form check
//! it here against the space's LinkML model, with one sentence per gap.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_norway::Value;

/// `spec.representations`: the mappings of the domain-standard representations (EP-125).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Representations {
    /// The iCalendar outputs (API/11 EP-159); none means no calendar is served. The single
    /// mapping object of version 1 loads as one output named `events` (deprecated).
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "ical_outputs"
    )]
    #[schemars(with = "Vec<IcalOutput>")]
    pub ical: Vec<IcalOutput>,
    /// The feed outputs (Atom, RSS 2.0, JSON Feed); none means no feed (EP-150, EP-151).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub feed: Vec<super::feed_output::FeedOutput>,
}

/// The most outputs of one format an Endpoint lists (owner 2026-10-22T09:30Z, EP-159).
pub const MAX_OUTPUTS: usize = 20;

/// The most model classes one output maps.
pub const MAX_SOURCES: usize = 10;

/// The types a preset serves (owner 2026-10-22T09:25Z); another type maps DTSTART and SUMMARY
/// itself.
pub const PRESET_TYPES: [&str; 3] = ["Event", "CouncilMeeting", "OfficeHours"];

/// The output name the deprecated single mapping object loads as, served at `events.ics`.
pub const LEGACY_OUTPUT: &str = "events";

/// One named calendar of `spec.representations.ical` (API/11 EP-159), served at
/// `/ical/v1/{name}.ics`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IcalOutput {
    /// The URL slug: lowercase letters, digits and `-`, 1 to 40, unique on the Endpoint.
    pub name: String,
    /// The contract version (API/11 EP-152); `1` when absent.
    #[serde(default = "first_version", skip_serializing_if = "is_first_version")]
    pub version: u64,
    /// The calendar's title per language tag (`NAME`, `X-WR-CALNAME`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub title: BTreeMap<String, String>,
    /// The calendar's description per language tag.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub description: BTreeMap<String, String>,
    /// The model classes the calendar maps, each with its preset or mapping; 1 to 10.
    pub sources: Vec<IcalMapping>,
    /// A query that narrows the view for this calendar, never widens it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<OutputFilter>,
    /// The language tag (BCP 47) a LanguageProperty is read in when a field names none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lang: Option<String>,
    /// `start` or `-start`: the order the first `limit` Events are kept in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sort: Option<String>,
    /// At most this many Events, 1 to 10000.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    /// The IANA zone a time without offset is read in; default `Europe/Bratislava`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
}

/// An output's NGSI-LD query on top of the view (TS 104 175 6.4.3.2): `q` and `scopeQ` are
/// AND-ed with a caller's, and a geo-query here cannot be replaced by one.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct OutputFilter {
    /// `q`, such as `eventCategory=="šport"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub q: Option<String>,
    /// `scopeQ`.
    #[serde(default, rename = "scopeQ", skip_serializing_if = "Option::is_none")]
    pub scope_q: Option<String>,
    /// `georel`; with `geometry` and `coordinates`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub georel: Option<String>,
    /// `geometry`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub geometry: Option<String>,
    /// `coordinates`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coordinates: Option<String>,
    /// `geoproperty`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub geoproperty: Option<String>,
}

fn ical_outputs<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<IcalOutput>, D::Error> {
    // By hand, so a wrong member is named in the error instead of "did not match any variant".
    struct Visitor;
    impl<'de> serde::de::Visitor<'de> for Visitor {
        type Value = Vec<IcalOutput>;
        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("a list of named outputs")
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            seq: A,
        ) -> Result<Vec<IcalOutput>, A::Error> {
            Vec::deserialize(serde::de::value::SeqAccessDeserializer::new(seq))
        }
        fn visit_map<A: serde::de::MapAccess<'de>>(
            self,
            map: A,
        ) -> Result<Vec<IcalOutput>, A::Error> {
            let mapping =
                IcalMapping::deserialize(serde::de::value::MapAccessDeserializer::new(map))?;
            Ok(vec![IcalOutput::legacy(mapping)])
        }
    }
    deserializer.deserialize_any(Visitor)
}

/// Each calendar's address under the Endpoint, as a record, a page head and a catalogue list
/// them: `(output, "ical/v1/{name}.ics")` (API/11 EP-159).
pub fn ical_links(outputs: &[IcalOutput]) -> Vec<(&IcalOutput, String)> {
    outputs
        .iter()
        .map(|output| (output, format!("ical/v1/{}.ics", output.name)))
        .collect()
}

impl IcalOutput {
    /// The title in `lang`, else the first, else the name: what a menu or a catalogue lists.
    pub fn label(&self, lang: &str) -> &str {
        self.title
            .get(lang)
            .or_else(|| self.title.values().next())
            .map_or(self.name.as_str(), String::as_str)
    }

    /// The output the deprecated single mapping object stands for.
    pub fn legacy(mapping: IcalMapping) -> Self {
        Self {
            name: LEGACY_OUTPUT.to_owned(),
            version: mapping.version,
            title: BTreeMap::new(),
            description: BTreeMap::new(),
            sources: vec![IcalMapping {
                version: ICAL_MAPPING_VERSION,
                ..mapping
            }],
            filter: None,
            lang: None,
            sort: None,
            limit: None,
            timezone: None,
        }
    }

    #[cfg(test)]
    fn renamed(self, name: &str) -> Self {
        Self {
            name: name.to_owned(),
            ..self
        }
    }

    /// Gaps the output has whatever the model, one sentence each, starting with
    /// `` `ical` output `name` ``.
    pub fn shape_gaps(&self) -> Vec<String> {
        let mut gaps = Vec::new();
        if !is_slug(&self.name) {
            gaps.push(
                "its name is not one of lowercase letters, digits and `-`, 1 to 40, starting and \
                 ending with a letter or digit, other than `preview`"
                    .to_owned(),
            );
        }
        if self.version != ICAL_MAPPING_VERSION {
            gaps.push(format!(
                "it is version {}, and this platform writes version {ICAL_MAPPING_VERSION}",
                self.version
            ));
        }
        for (member, texts) in [("title", &self.title), ("description", &self.description)] {
            if texts
                .iter()
                .any(|(tag, text)| !is_tag(tag) || !is_plain(text, 1000))
            {
                gaps.push(format!(
                    "`{member}` takes a language tag (`sk`, `en-GB`) to 1 to 1000 characters \
                     without control characters"
                ));
            }
        }
        if self.sources.is_empty() || self.sources.len() > MAX_SOURCES {
            gaps.push(format!("it needs 1 to {MAX_SOURCES} sources"));
        }
        if let Some(filter) = &self.filter {
            gaps.extend(filter.gaps());
        }
        if let Some(lang) = self.lang.as_deref().filter(|l| !is_tag(l)) {
            gaps.push(format!("`lang: {lang}` is not a language tag such as `sk`"));
        }
        if let Some(sort) = self
            .sort
            .as_deref()
            .filter(|s| !matches!(*s, "start" | "-start"))
        {
            gaps.push(format!("`sort: {sort}` is neither `start` nor `-start`"));
        }
        if self.limit.is_some_and(|n| n == 0 || n > 10_000) {
            gaps.push("`limit` must be 1 to 10000".to_owned());
        }
        if let Some(zone) = self.timezone.as_deref().filter(|z| !is_zone(z)) {
            gaps.push(format!("`timezone: {zone}` is not an IANA zone name"));
        }
        let mut gaps = self.named(gaps);
        for source in &self.sources {
            gaps.extend(self.named(source.shape_gaps()));
            if source.base().is_some() && !PRESET_TYPES.contains(&source.entity_type.as_str()) {
                gaps.push(self.name_one(format!(
                    "`ical` uses a preset for {}; a preset serves {} alone, so map DTSTART and SUMMARY",
                    source.entity_type,
                    PRESET_TYPES.join(", ")
                )));
            }
        }
        gaps
    }

    /// Every gap of the output against the space's LinkML model (`None`: the space has none).
    pub fn model_gaps(&self, linkml: Option<&str>) -> std::result::Result<Vec<String>, String> {
        let gaps = self.shape_gaps();
        if !gaps.is_empty() {
            return Ok(gaps);
        }
        let mut gaps = Vec::new();
        for source in &self.sources {
            gaps.extend(self.named(source.model_gaps(linkml)?));
        }
        Ok(gaps)
    }

    /// A source's sentence (`` `ical` maps no DTSTART ``) or the output's own, with the name.
    fn name_one(&self, gap: String) -> String {
        match gap.strip_prefix("`ical` ") {
            Some(rest) => format!("`ical` output `{}` {rest}", self.name),
            None => format!("`ical` output `{}`: {gap}", self.name),
        }
    }

    fn named(&self, gaps: Vec<String>) -> Vec<String> {
        gaps.into_iter().map(|gap| self.name_one(gap)).collect()
    }
}

impl OutputFilter {
    /// Whether no member is set.
    pub fn is_empty(&self) -> bool {
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

    /// What the filter fails, one sentence each naming `filter.<member>` (API/11 EP-159). Each
    /// member goes through the broker's own parser (`ngsild-ql`), so a filter the broker would
    /// refuse at serve, and an output that would then serve unfiltered, is refused at save.
    pub fn gaps(&self) -> Vec<String> {
        let mut gaps = Vec::new();
        let entries = self.entries();
        for (member, _) in entries
            .iter()
            .filter(|(_, v)| v.is_some_and(|v| !is_plain(v, 2000)))
        {
            gaps.push(format!(
                "`filter.{member}` must be 1 to 2000 characters without control characters"
            ));
        }
        if !gaps.is_empty() {
            return gaps;
        }
        if let Some(Err(why)) = self.q.as_deref().map(antares_ql::parse_q) {
            gaps.push(format!("`filter.q` is no NGSI-LD query: {why}"));
        }
        if let Some(Err(why)) = self
            .scope_q
            .as_deref()
            .map(antares_ql::scope::check_scope_q)
        {
            gaps.push(format!("`filter.scopeQ` is no NGSI-LD scope query: {why}"));
        }
        let geo = [&self.georel, &self.geometry, &self.coordinates];
        if geo.iter().any(|v| v.is_some()) && !geo.iter().all(|v| v.is_some()) {
            gaps.push(
                "`filter` names a geo-query: it needs georel, geometry and coordinates".to_owned(),
            );
        } else if self.georel.is_some() {
            let params = entries
                .iter()
                .filter(|(member, _)| !matches!(*member, "q" | "scopeQ"))
                .filter_map(|(member, v)| v.map(|v| ((*member).to_owned(), v.to_owned())))
                .collect();
            if let Err(why) = antares_ql::geo::GeoQuery::from_params(&params) {
                gaps.push(format!("`filter.georel` is no NGSI-LD geo-query: {why}"));
            }
        }
        match self.geoproperty.as_deref() {
            Some(_) if self.georel.is_none() => {
                gaps.push("`filter.geoproperty` belongs to a geo-query".to_owned());
            }
            Some(name) if !is_name(name) => {
                gaps.push(format!(
                    "`filter.geoproperty: {name}` is not an attribute name"
                ));
            }
            _ => {}
        }
        if self.is_empty() {
            gaps.push("`filter` is empty: name q, scopeQ or a geo-query, or drop it".to_owned());
        }
        gaps
    }
}

/// Gaps of an Endpoint's iCal output list: the count and the names (EP-159).
pub fn outputs_gaps(outputs: &[IcalOutput]) -> Vec<String> {
    let mut gaps = Vec::new();
    if outputs.len() > MAX_OUTPUTS {
        gaps.push(format!(
            "`ical` lists {} outputs, at most {MAX_OUTPUTS}",
            outputs.len()
        ));
    }
    let mut seen = BTreeSet::new();
    for output in outputs {
        if !seen.insert(output.name.as_str()) {
            gaps.push(format!(
                "`ical` lists the output name `{}` twice; each name is one URL",
                output.name
            ));
        }
        gaps.extend(output.shape_gaps());
    }
    gaps
}

/// One source of an [`IcalOutput`] (`spec.representations.ical[].sources[]`), and the
/// deprecated single mapping object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct IcalMapping {
    /// The contract version (API/11 EP-152); `1` when absent, the one version the broker reads.
    #[serde(default = "first_version", skip_serializing_if = "is_first_version")]
    pub version: u64,
    /// The NGSI-LD type the feed maps.
    #[serde(rename = "type", default = "event")]
    pub entity_type: String,
    /// The preset the mapping starts from; with no `mapping` either, `sdm-event`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<Preset>,
    /// Fields that replace the preset's, or the whole mapping when no preset is named.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub mapping: BTreeMap<IcalField, FieldSource>,
}

fn event() -> String {
    "Event".to_owned()
}

/// The `ical` contract version this platform writes and the broker reads (API/11 EP-152).
pub const ICAL_MAPPING_VERSION: u64 = 1;

fn first_version() -> u64 {
    ICAL_MAPPING_VERSION
}

fn is_first_version(v: &u64) -> bool {
    *v == ICAL_MAPPING_VERSION
}

impl Default for IcalMapping {
    fn default() -> Self {
        Self {
            version: ICAL_MAPPING_VERSION,
            entity_type: event(),
            preset: None,
            mapping: BTreeMap::new(),
        }
    }
}

/// A named starting mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum Preset {
    /// Smart Data Models `Event` (dataModel.Event), the default.
    SdmEvent,
    /// schema.org `Event`.
    SchemaOrg,
}

/// The RFC 5545 VEVENT properties a mapping may name (§3.6.1, §3.8).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "SCREAMING-KEBAB-CASE")]
pub enum IcalField {
    /// §3.8.4.7, fixed: the entity id.
    Uid,
    /// §3.8.7.2, fixed: the entity's `modifiedAt`.
    Dtstamp,
    /// §3.8.2.4.
    Dtstart,
    /// §3.8.2.2.
    Dtend,
    /// §3.8.2.5.
    Duration,
    /// §3.8.1.12.
    Summary,
    /// §3.8.1.5.
    Description,
    /// §3.8.1.7.
    Location,
    /// §3.8.1.6.
    Geo,
    /// §3.8.4.6.
    Url,
    /// §3.8.1.2.
    Categories,
    /// §3.8.1.11.
    Status,
    /// §3.8.4.3.
    Organizer,
    /// §3.8.7.3.
    LastModified,
    /// §3.8.1.3, `PUBLIC` only.
    Class,
}

/// What a field's source must be, for the checks and the generated table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Accepts {
    /// `date` or `datetime`.
    Date,
    /// A string, an enum or a LanguageProperty.
    Text,
    /// Text or a structured value (a postal address).
    Place,
    /// A GeoProperty.
    Geo,
    /// `uri`.
    Uri,
    /// A string (an ISO 8601 duration).
    Duration,
    /// Only a constant.
    Constant,
    /// Not mappable.
    Fixed,
}

impl Accepts {
    /// The ranges, in the words a refusal uses.
    pub const fn words(self) -> &'static str {
        match self {
            Self::Date => "date or datetime",
            Self::Text => "a string, an enum or a LanguageProperty",
            Self::Place => "a string, an enum, a LanguageProperty or a structured address",
            Self::Geo => "a GeoProperty",
            Self::Uri => "uri",
            Self::Duration => "a string",
            Self::Constant => "a constant",
            Self::Fixed => "nothing: the platform sets it",
        }
    }
}

/// The remap keys of [`FieldRemap`], by their manifest names.
const REMAPS: [&str; 9] = [
    "firstOf",
    "join",
    "constant",
    "language",
    "date",
    "timezone",
    "endInclusive",
    "map",
    "truncate",
];

impl IcalField {
    /// Every field, in table order.
    pub const ALL: [Self; 15] = [
        Self::Uid,
        Self::Dtstamp,
        Self::Dtstart,
        Self::Summary,
        Self::Dtend,
        Self::Duration,
        Self::Description,
        Self::Location,
        Self::Geo,
        Self::Url,
        Self::Categories,
        Self::Status,
        Self::Organizer,
        Self::LastModified,
        Self::Class,
    ];

    /// The property name as RFC 5545 writes it.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Uid => "UID",
            Self::Dtstamp => "DTSTAMP",
            Self::Dtstart => "DTSTART",
            Self::Dtend => "DTEND",
            Self::Duration => "DURATION",
            Self::Summary => "SUMMARY",
            Self::Description => "DESCRIPTION",
            Self::Location => "LOCATION",
            Self::Geo => "GEO",
            Self::Url => "URL",
            Self::Categories => "CATEGORIES",
            Self::Status => "STATUS",
            Self::Organizer => "ORGANIZER",
            Self::LastModified => "LAST-MODIFIED",
            Self::Class => "CLASS",
        }
    }

    /// DTSTART by RFC 5545; SUMMARY by the platform, as an entry without a title is of no use.
    /// UID and DTSTAMP are required too, and fixed.
    pub const fn required(self) -> bool {
        matches!(self, Self::Dtstart | Self::Summary)
    }

    /// What the field's source must be.
    pub const fn accepts(self) -> Accepts {
        match self {
            Self::Uid | Self::Dtstamp => Accepts::Fixed,
            Self::Dtstart | Self::Dtend | Self::LastModified => Accepts::Date,
            Self::Summary | Self::Description | Self::Categories | Self::Status => Accepts::Text,
            Self::Location => Accepts::Place,
            Self::Geo => Accepts::Geo,
            Self::Url => Accepts::Uri,
            Self::Organizer => Accepts::Uri,
            Self::Duration => Accepts::Duration,
            Self::Class => Accepts::Constant,
        }
    }

    /// Whether the field takes the remap `key` (one of `REMAPS`).
    pub fn allows(self, key: &str) -> bool {
        let text = matches!(self.accepts(), Accepts::Text | Accepts::Place);
        match key {
            "firstOf" => !matches!(self.accepts(), Accepts::Fixed | Accepts::Constant),
            "join" => text && self != Self::Status,
            "constant" => matches!(self, Self::Class | Self::Categories | Self::Duration),
            "language" => text,
            "date" => self.accepts() == Accepts::Date,
            "timezone" => matches!(self, Self::Dtstart | Self::Dtend),
            "endInclusive" => self == Self::Dtend,
            "map" => matches!(self, Self::Status | Self::Categories),
            "truncate" => matches!(self, Self::Summary | Self::Description),
            _ => false,
        }
    }

    /// The remaps the field takes, for the generated table.
    pub fn remaps(self) -> Vec<&'static str> {
        REMAPS.into_iter().filter(|k| self.allows(k)).collect()
    }
}

impl fmt::Display for IcalField {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How a date field reads its value.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum DateMode {
    /// As the value is: a date stays a date, a date-time a date-time.
    #[default]
    Auto,
    /// A whole day (`VALUE=DATE`).
    Date,
    /// A date-time.
    Datetime,
}

/// A field's source: an attribute name, or an object with one source and its remaps.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(untagged)]
pub enum FieldSource {
    /// `DTSTART: startDate`.
    Attribute(String),
    /// `SUMMARY: {attribute: name, language: sk}`.
    Remap(Box<FieldRemap>),
}

impl<'de> Deserialize<'de> for FieldSource {
    // By hand, so a wrong key is named in the error instead of "did not match any variant".
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = FieldSource;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("an attribute name or an object of a source and its remaps")
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<FieldSource, E> {
                Ok(FieldSource::Attribute(v.to_owned()))
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                map: A,
            ) -> Result<FieldSource, A::Error> {
                FieldRemap::deserialize(serde::de::value::MapAccessDeserializer::new(map))
                    .map(|remap| FieldSource::Remap(Box::new(remap)))
            }
        }
        deserializer.deserialize_any(Visitor)
    }
}

/// The object form of a [`FieldSource`] (API/02 §8b): exactly one of `attribute`, `firstOf`,
/// `join` and `constant`, and the remaps the field allows.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct FieldRemap {
    /// An attribute, or a dotted path into its value (`address.addressLocality`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attribute: Option<String>,
    /// The first of these attributes that has a value.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub first_of: Vec<String>,
    /// The values of these attributes that have one, joined by `separator`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub join: Vec<String>,
    /// The `join` separator; default `", "`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub separator: Option<String>,
    /// A fixed value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub constant: Option<String>,
    /// Which text of a LanguageProperty: a two-letter code or `first` (the default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// How a date field reads its value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub date: Option<DateMode>,
    /// The IANA zone of a floating time; default `Europe/Bratislava`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
    /// The source names the last day, so DTEND is the day after.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub end_inclusive: bool,
    /// Source value to calendar value.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub map: BTreeMap<String, String>,
    /// The calendar value of a source value `map` does not name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    /// At most this many characters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncate: Option<u32>,
}

impl FieldSource {
    fn attribute(name: &str) -> Self {
        Self::Attribute(name.to_owned())
    }

    /// The object form; an attribute name is `{attribute: name}`.
    pub fn remap(&self) -> FieldRemap {
        match self {
            Self::Attribute(name) => FieldRemap {
                attribute: Some(name.clone()),
                ..FieldRemap::default()
            },
            Self::Remap(remap) => (**remap).clone(),
        }
    }
}

/// The STATUS values RFC 5545 §3.8.1.11 gives a VEVENT.
const STATUS_VALUES: [&str; 3] = ["TENTATIVE", "CONFIRMED", "CANCELLED"];

impl Preset {
    /// Every preset.
    pub const ALL: [Self; 2] = [Self::SdmEvent, Self::SchemaOrg];

    /// The manifest name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SdmEvent => "sdm-event",
            Self::SchemaOrg => "schema-org",
        }
    }

    /// The preset's mapping.
    pub fn mapping(self) -> BTreeMap<IcalField, FieldSource> {
        use IcalField as F;
        let mut fields: BTreeMap<_, _> = [
            (F::Dtstart, "startDate"),
            (F::Dtend, "endDate"),
            (F::Summary, "name"),
            (F::Description, "description"),
            (F::Location, "address"),
            (F::Geo, "location"),
            (F::Url, "url"),
        ]
        .into_iter()
        .map(|(field, attribute)| (field, FieldSource::attribute(attribute)))
        .collect();
        if self == Self::SchemaOrg {
            let status = |value: &str| format!("https://schema.org/{value}");
            let map = [
                ("EventScheduled", "CONFIRMED"),
                ("EventRescheduled", "CONFIRMED"),
                ("EventMovedOnline", "CONFIRMED"),
                ("EventPostponed", "TENTATIVE"),
                ("EventCancelled", "CANCELLED"),
            ]
            .into_iter()
            .flat_map(|(from, to)| [(from.to_owned(), to), (status(from), to)])
            .map(|(from, to)| (from, to.to_owned()))
            .collect();
            fields.insert(
                F::Status,
                FieldSource::Remap(Box::new(FieldRemap {
                    attribute: Some("eventStatus".to_owned()),
                    map,
                    default: Some("CONFIRMED".to_owned()),
                    ..FieldRemap::default()
                })),
            );
        }
        fields
    }
}

impl IcalMapping {
    /// The preset the mapping starts from, if any: the named one, or `sdm-event` when nothing
    /// is written.
    pub fn base(&self) -> Option<Preset> {
        match (self.preset, self.mapping.is_empty()) {
            (Some(preset), _) => Some(preset),
            (None, true) => Some(Preset::SdmEvent),
            (None, false) => None,
        }
    }

    /// The mapping in force: the preset's fields with the written ones over them.
    pub fn effective(&self) -> BTreeMap<IcalField, FieldSource> {
        let mut fields = self.base().map(Preset::mapping).unwrap_or_default();
        fields.extend(self.mapping.clone());
        fields
    }

    /// Gaps a mapping has whatever the model: one sentence per gap, each starting with
    /// `` `ical` ``. Empty when the mapping is well formed.
    pub fn shape_gaps(&self) -> Vec<String> {
        let mut gaps = Vec::new();
        if self.version != ICAL_MAPPING_VERSION {
            gaps.push(format!(
                "`ical` is version {}, and this platform writes version {ICAL_MAPPING_VERSION}",
                self.version
            ));
        }
        if !is_name(&self.entity_type) {
            gaps.push(format!(
                "`ical` names the type `{}`, which is not an NGSI-LD type name",
                self.entity_type
            ));
        }
        if self.mapping.contains_key(&IcalField::Dtend)
            && self.mapping.contains_key(&IcalField::Duration)
        {
            gaps.push("`ical` maps both DTEND and DURATION; RFC 5545 allows one".to_owned());
        }
        for (field, source) in &self.mapping {
            gaps.extend(field_shape_gaps(*field, &source.remap()));
        }
        gaps
    }

    /// Every gap of the mapping against the space's LinkML model (`None`: the space has none),
    /// one sentence each (API/02 §8b). `Err` when the model does not parse.
    pub fn model_gaps(&self, linkml: Option<&str>) -> std::result::Result<Vec<String>, String> {
        let mut gaps = self.shape_gaps();
        if !gaps.is_empty() {
            return Ok(gaps);
        }
        let schema: Value = match linkml {
            Some(text) => serde_norway::from_str(text).map_err(|err| err.to_string())?,
            None => Value::Null,
        };
        let model = Model { schema: &schema };
        let class = &self.entity_type;
        if model.class(class).is_none() {
            return Ok(vec![format!(
                "`ical` maps the type {class}, which the data model does not have"
            )]);
        }
        let effective = self.effective();
        for field in IcalField::ALL.into_iter().filter(|f| f.required()) {
            if !effective.contains_key(&field) {
                gaps.push(format!(
                    "`ical` maps no {field}, which every calendar entry needs"
                ));
            }
        }
        if effective.contains_key(&IcalField::Dtend) && effective.contains_key(&IcalField::Duration)
        {
            gaps.push("`ical` maps both DTEND and DURATION; RFC 5545 allows one".to_owned());
        }
        for (field, source) in &effective {
            let written = self.mapping.contains_key(field);
            gaps.extend(field_model_gaps(
                &model,
                class,
                *field,
                &source.remap(),
                written,
            ));
        }
        Ok(gaps)
    }
}

/// An NGSI-LD attribute or type name: a letter or `_`, then letters, digits and `_`.
pub(super) fn is_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
        && name.len() <= 128
}

/// A dotted path of at most three names.
pub(super) fn is_path(path: &str) -> bool {
    let segments: Vec<_> = path.split('.').collect();
    segments.len() <= 3 && segments.iter().all(|s| is_name(s))
}

/// A short plain value: no control characters, at most `max` characters.
pub(super) fn is_plain(value: &str, max: usize) -> bool {
    !value.is_empty() && value.chars().count() <= max && !value.chars().any(char::is_control)
}

fn field_shape_gaps(field: IcalField, remap: &FieldRemap) -> Vec<String> {
    let texts = match field {
        IcalField::Uid => vec!["UID is fixed to the entity id and cannot be mapped".to_owned()],
        IcalField::Dtstamp => {
            vec![
                "DTSTAMP is fixed (the mapped LAST-MODIFIED, else the newest modifiedAt of the \
                 mapped attributes) and cannot be mapped"
                    .to_owned(),
            ]
        }
        _ => [
            source_gaps(field, remap),
            value_gaps(field, remap),
            status_gaps(field, remap),
        ]
        .concat(),
    };
    texts
        .into_iter()
        .map(|text| format!("`ical` {field}: {text}"))
        .collect()
}

/// One source, attribute paths only, and only the remaps the field takes.
fn source_gaps(field: IcalField, remap: &FieldRemap) -> Vec<String> {
    let mut gaps = Vec::new();
    let sources = [
        remap.attribute.is_some(),
        !remap.first_of.is_empty(),
        !remap.join.is_empty(),
        remap.constant.is_some(),
    ];
    if sources.iter().filter(|s| **s).count() != 1 {
        gaps.push("needs exactly one of attribute, firstOf, join or constant".to_owned());
    }
    let used = [
        ("firstOf", !remap.first_of.is_empty()),
        ("join", !remap.join.is_empty()),
        ("constant", remap.constant.is_some()),
        ("language", remap.language.is_some()),
        ("date", remap.date.is_some()),
        ("timezone", remap.timezone.is_some()),
        ("endInclusive", remap.end_inclusive),
        ("map", !remap.map.is_empty() || remap.default.is_some()),
        ("truncate", remap.truncate.is_some()),
    ];
    for (key, _) in used.iter().filter(|(key, on)| *on && !field.allows(key)) {
        let keys = field.remaps();
        gaps.push(format!(
            "does not take `{key}`; it takes {}",
            if keys.is_empty() {
                "none".to_owned()
            } else {
                keys.join(", ")
            }
        ));
    }
    if remap.separator.is_some() && remap.join.is_empty() {
        gaps.push("`separator` belongs to `join`".to_owned());
    }
    for path in remap
        .attribute
        .iter()
        .chain(&remap.first_of)
        .chain(&remap.join)
        .filter(|p| !is_path(p))
    {
        gaps.push(format!(
            "`{path}` is not an attribute path (names joined by dots, at most three)"
        ));
    }
    gaps
}

/// The values the remaps carry: short plain text, a language code, a zone name, a sane length.
fn value_gaps(field: IcalField, remap: &FieldRemap) -> Vec<String> {
    let mut gaps = Vec::new();
    if remap.separator.as_deref().is_some_and(|s| !is_plain(s, 10)) {
        gaps.push("`separator` must be 1 to 10 characters, no control characters".to_owned());
    }
    if field == IcalField::Class {
        if remap.constant.as_deref() != Some("PUBLIC") {
            gaps.push("CLASS takes only `constant: PUBLIC`".to_owned());
        }
    } else if remap.constant.as_deref().is_some_and(|c| !is_plain(c, 100)) {
        gaps.push("`constant` must be 1 to 100 characters, no control characters".to_owned());
    }
    if let Some(language) = remap.language.as_deref().filter(|l| !is_language(l)) {
        gaps.push(format!(
            "`language: {language}` is neither a language tag (`sk`, `en-GB`) nor `first`"
        ));
    }
    if field == IcalField::Duration && remap.constant.as_deref().is_some_and(|c| !is_duration(c)) {
        gaps.push("`constant` must be an RFC 5545 duration such as PT2H or P1D".to_owned());
    }
    if let Some(zone) = remap.timezone.as_deref().filter(|z| !is_zone(z)) {
        gaps.push(format!("`timezone: {zone}` is not an IANA zone name"));
    }
    if remap.truncate.is_some_and(|n| n == 0 || n > 10_000) {
        gaps.push("`truncate` must be 1 to 10000".to_owned());
    }
    if remap
        .map
        .iter()
        .any(|(from, to)| !is_plain(from, 200) || !is_plain(to, 100))
    {
        gaps.push("`map` keys and values must be short plain text".to_owned());
    }
    gaps
}

/// STATUS maps only to the values RFC 5545 gives a VEVENT.
fn status_gaps(field: IcalField, remap: &FieldRemap) -> Vec<String> {
    if field != IcalField::Status {
        return Vec::new();
    }
    let bad: BTreeSet<&str> = remap
        .map
        .values()
        .chain(&remap.default)
        .map(String::as_str)
        .filter(|v| !STATUS_VALUES.contains(v))
        .collect();
    if bad.is_empty() {
        return Vec::new();
    }
    vec![format!(
        "STATUS takes TENTATIVE, CONFIRMED or CANCELLED, not {}",
        bad.into_iter().collect::<Vec<_>>().join(", ")
    )]
}

/// A language tag or `first`.
pub(super) fn is_language(language: &str) -> bool {
    language == "first" || is_tag(language)
}

/// A BCP 47 language tag by its shape: a primary subtag of 2 or 3 letters, then subtags of 1 to
/// 8 letters or digits.
fn is_tag(tag: &str) -> bool {
    let mut subtags = tag.split('-');
    subtags
        .next()
        .is_some_and(|p| (2..=3).contains(&p.len()) && p.chars().all(|c| c.is_ascii_alphabetic()))
        && subtags
            .all(|s| (1..=8).contains(&s.len()) && s.chars().all(|c| c.is_ascii_alphanumeric()))
        && tag.len() <= 35
}

/// An output name: `[a-z0-9-]`, 1 to 40, a letter or digit at both ends, never `preview` (the
/// broker's preview route).
fn is_slug(name: &str) -> bool {
    let edge = |c: Option<char>| c.is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    name.len() <= 40
        && edge(name.chars().next())
        && edge(name.chars().last())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && name != "preview"
}

/// An RFC 5545 §3.3.6 duration: `P` then weeks, or days and a time of hours, minutes, seconds.
fn is_duration(value: &str) -> bool {
    fn units(mut text: &str, order: &[char]) -> Option<usize> {
        let mut found = 0;
        for unit in order {
            let digits = text.chars().take_while(char::is_ascii_digit).count();
            if digits > 0 && text[digits..].starts_with(*unit) {
                text = &text[digits + 1..];
                found += 1;
            }
        }
        text.is_empty().then_some(found)
    }
    let Some(rest) = value.strip_prefix('P') else {
        return false;
    };
    if units(rest, &['W']) == Some(1) {
        return true;
    }
    let (date, time) = rest
        .split_once('T')
        .map_or((rest, None), |(d, t)| (d, Some(t)));
    let days = units(date, &['D']);
    let times = time.map(|t| units(t, &['H', 'M', 'S']));
    match (days, times) {
        (Some(_), Some(Some(n))) => n > 0,
        (Some(n), None) => n > 0,
        _ => false,
    }
}

/// `UTC` or an IANA name such as `Europe/Bratislava`, by its shape.
fn is_zone(zone: &str) -> bool {
    zone == "UTC"
        || (zone.contains('/')
            && is_plain(zone, 64)
            && zone
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "/_+-".contains(c)))
}

/// What the model says of one attribute.
pub(super) struct Slot {
    range: String,
    kind: Option<String>,
    required: bool,
}

impl Slot {
    fn core(range: &str, kind: Option<&str>, required: bool) -> Self {
        Self {
            range: range.to_owned(),
            kind: kind.map(str::to_owned),
            required,
        }
    }
}

/// Attributes every NGSI-LD entity has, so a model never declares them.
fn core_slot(name: &str) -> Option<Slot> {
    match name {
        "id" => Some(Slot::core("uri", None, true)),
        "type" => Some(Slot::core("string", None, true)),
        "createdAt" | "modifiedAt" => Some(Slot::core("datetime", None, true)),
        "location" | "observationSpace" | "operationSpace" => {
            Some(Slot::core("GeoJSON", Some("GeoProperty"), false))
        }
        _ => None,
    }
}

/// A LinkML schema, read as YAML.
pub(super) struct Model<'a> {
    pub(super) schema: &'a Value,
}

impl Model<'_> {
    pub(super) fn class(&self, name: &str) -> Option<&Value> {
        self.schema.get("classes")?.get(name)
    }

    fn enum_values(&self, name: &str) -> Option<Vec<String>> {
        let values = self
            .schema
            .get("enums")?
            .get(name)?
            .get("permissible_values")?;
        Some(match values {
            Value::Mapping(m) => m
                .keys()
                .filter_map(|k| k.as_str().map(str::to_owned))
                .collect(),
            Value::Sequence(s) => s
                .iter()
                .filter_map(|k| k.as_str().map(str::to_owned))
                .collect(),
            _ => Vec::new(),
        })
    }

    /// The slot `name` of `class` (its own, then its `is_a` ancestors'), with its range, its
    /// NGSI-LD kind and whether it is required.
    fn slot(&self, class: &str, name: &str) -> Option<Slot> {
        let mut current = class.to_owned();
        for _ in 0..8 {
            let declared = self.class(&current)?;
            let field = |node: Option<&Value>, key: &str| node.and_then(|n| n.get(key)).cloned();
            let usage = declared.get("slot_usage").and_then(|u| u.get(name));
            let attribute = declared.get("attributes").and_then(|a| a.get(name));
            let listed = declared
                .get("slots")
                .and_then(Value::as_sequence)
                .is_some_and(|s| s.iter().any(|s| s.as_str() == Some(name)));
            if listed || attribute.is_some() {
                let global = listed
                    .then(|| self.schema.get("slots").and_then(|s| s.get(name)))
                    .flatten();
                let pick = |key: &str| {
                    field(usage, key)
                        .or_else(|| field(attribute, key))
                        .or_else(|| field(global, key))
                };
                let range = pick("range")
                    .and_then(|v| v.as_str().map(str::to_owned))
                    .or_else(|| {
                        self.schema
                            .get("default_range")
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                    })
                    .unwrap_or_else(|| "string".to_owned());
                let kind = pick("annotations")
                    .and_then(|a| a.get("ngsi_ld_kind").cloned())
                    .and_then(|k| match k {
                        Value::String(s) => Some(s),
                        // `{tag: ngsi_ld_kind, value: GeoProperty}`
                        other => other
                            .get("value")
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                    });
                let required = pick("required").and_then(|v| v.as_bool()) == Some(true);
                return Some(Slot {
                    range,
                    kind,
                    required,
                });
            }
            current = declared.get("is_a")?.as_str()?.to_owned();
        }
        None
    }
}

/// Resolves a dotted path from `class`: the slot it ends at, or the sentence that says why not.
pub(super) fn resolve(model: &Model, class: &str, path: &str) -> std::result::Result<Slot, String> {
    let mut segments = path.split('.');
    let first = segments.next().unwrap_or_default();
    let mut slot = match model.slot(class, first).or_else(|| core_slot(first)) {
        Some(slot) => slot,
        None => return Err(format!("`{first}`, which {class} does not have")),
    };
    let mut owner = class.to_owned();
    for segment in segments {
        if model.class(&slot.range).is_none() {
            return Err(format!(
                "`{path}`, but `{owner}`'s value has no members (range {})",
                slot.range
            ));
        }
        owner = slot.range.clone();
        let inner = model
            .slot(&owner, segment)
            .ok_or_else(|| format!("`{path}`, and {owner} has no `{segment}`"))?;
        slot = Slot {
            required: slot.required && inner.required,
            ..inner
        };
    }
    Ok(slot)
}

/// Whether a slot's value fits what a field accepts.
pub(super) fn fits(model: &Model, slot: &Slot, accepts: Accepts) -> bool {
    let range = slot.range.as_str();
    let kind = slot.kind.as_deref();
    if kind == Some("Relationship") {
        return false;
    }
    let text =
        kind == Some("LanguageProperty") || range == "string" || model.enum_values(range).is_some();
    match accepts {
        Accepts::Date => matches!(range, "date" | "datetime") && kind != Some("LanguageProperty"),
        Accepts::Text | Accepts::Duration => text,
        Accepts::Place => text || model.class(range).is_some(),
        Accepts::Geo => kind == Some("GeoProperty"),
        Accepts::Uri => matches!(range, "uri" | "uriorcurie" | "url"),
        Accepts::Constant | Accepts::Fixed => false,
    }
}

fn describe(model: &Model, slot: &Slot) -> String {
    match slot.kind.as_deref() {
        Some(kind @ ("LanguageProperty" | "GeoProperty" | "Relationship")) => {
            format!("a {kind}")
        }
        _ if model.enum_values(&slot.range).is_some() => format!("the enum {}", slot.range),
        _ => format!("range {}", slot.range),
    }
}

fn field_model_gaps(
    model: &Model,
    class: &str,
    field: IcalField,
    remap: &FieldRemap,
    written: bool,
) -> Vec<String> {
    let mut gaps = Vec::new();
    let paths: Vec<&String> = remap
        .attribute
        .iter()
        .chain(&remap.first_of)
        .chain(&remap.join)
        .collect();
    // A preset's optional field is left out when the type lacks its attribute, and its values
    // are checked one by one at serve (T-4901): a model's plain `string` url still renders.
    if !written && !field.required() {
        return gaps;
    }
    let mut resolved = Vec::new();
    for path in &paths {
        match resolve(model, class, path) {
            Ok(slot) => resolved.push(slot),
            Err(why) => gaps.push(format!("`ical` maps {field} to {why}")),
        }
    }
    if !gaps.is_empty() {
        return gaps;
    }
    for (path, slot) in paths.iter().zip(&resolved) {
        if !fits(model, slot, field.accepts()) {
            gaps.push(format!(
                "`ical` maps {field} to `{path}`, {}; {field} takes {}",
                describe(model, slot),
                field.accepts().words()
            ));
        }
    }
    if field.required() && !resolved.is_empty() {
        let required = if remap.join.is_empty() {
            resolved.last().is_some_and(|s| s.required)
        } else {
            resolved.iter().any(|s| s.required)
        };
        if !required {
            let path = paths.last().map(|p| p.as_str()).unwrap_or_default();
            gaps.push(format!(
                "`ical` maps {field} to `{path}`, which is not a required attribute of {class}: \
                 mark it required or end a firstOf with a required one"
            ));
        }
    }
    if remap.default.is_none() {
        let mut missing = BTreeSet::new();
        for slot in &resolved {
            let Some(values) = model.enum_values(&slot.range) else {
                continue;
            };
            if remap.map.is_empty() && field != IcalField::Status {
                continue;
            }
            missing.extend(values.into_iter().filter(|v| !remap.map.contains_key(v)));
        }
        if !missing.is_empty() {
            gaps.push(format!(
                "`ical` maps {field} from an enum without {}: map each value or name a default",
                missing.into_iter().collect::<Vec<_>>().join(", ")
            ));
        }
    }
    gaps
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shared fixture cases (`testdata/ical-mapping`), run beside the code so the code graph
    /// sees each check tested; `tests/ical_mapping_tests.rs` runs them through the public API.
    #[test]
    fn the_fixture_cases_report_their_gaps() {
        #[derive(Deserialize)]
        struct Case {
            ical: IcalMapping,
            gaps: Vec<String>,
        }
        let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../../testdata/ical-mapping/");
        let read = |file: &str| std::fs::read_to_string(format!("{root}{file}")).expect(file);
        let model = read("model.linkml.yaml");
        let cases: BTreeMap<String, Case> =
            serde_norway::from_str(&read("cases.yaml")).expect("cases parse");
        for (name, case) in cases {
            let gaps = case.ical.model_gaps(Some(&model)).expect("model parses");
            assert_eq!(gaps, case.gaps, "case {name}");
        }
    }

    #[test]
    fn field_names_are_the_wire_names() {
        for field in IcalField::ALL {
            let wire = serde_json::to_value(field).expect("serializes");
            assert_eq!(wire.as_str(), Some(field.as_str()), "{field:?}");
        }
        assert!(is_language("sk") && is_language("first") && is_language("en-GB"));
        assert!(!is_language("s") && !is_language("sk_SK") && !is_language("sk-"));
        for ok in ["PT2H", "P1D", "P1W", "P1DT2H30M", "PT45M"] {
            assert!(is_duration(ok), "{ok}");
        }
        for bad in [
            "P",
            "PT",
            "2H",
            "P1H",
            "PT1D",
            "P1W2D",
            "P-1D",
            "PT2H\r\nX:y",
        ] {
            assert!(!is_duration(bad), "{bad}");
        }
        assert!(is_slug("sport") && is_slug("okres-zvolen") && is_slug("7"));
        for bad in ["", "-a", "a-", "Sport", "a_b", "preview", &"a".repeat(41)] {
            assert!(!is_slug(bad), "{bad}");
        }
        assert!(is_zone("Europe/Bratislava") && is_zone("UTC") && !is_zone("CET; x"));
    }

    #[test]
    fn outputs_are_checked_by_count_name_and_members() {
        let one = |name: &str| {
            IcalOutput::legacy(IcalMapping {
                entity_type: "CouncilMeeting".into(),
                ..IcalMapping::default()
            })
            .renamed(name)
        };
        assert_eq!(outputs_gaps(&[one("a"), one("b")]), Vec::<String>::new());
        assert_eq!(
            outputs_gaps(&[one("a"), one("a")]),
            ["`ical` lists the output name `a` twice; each name is one URL"]
        );
        let many: Vec<IcalOutput> = (0..=MAX_OUTPUTS).map(|i| one(&format!("o{i}"))).collect();
        assert_eq!(outputs_gaps(&many).len(), 1);
        let mut bad = one("a");
        bad.filter = Some(OutputFilter {
            geoproperty: Some("location".into()),
            ..OutputFilter::default()
        });
        bad.sources[0].entity_type = "Article".into();
        assert_eq!(
            bad.shape_gaps(),
            [
                "`ical` output `a`: `filter.geoproperty` belongs to a geo-query",
                "`ical` output `a` uses a preset for Article; a preset serves Event, \
                 CouncilMeeting, OfficeHours alone, so map DTSTART and SUMMARY",
            ]
        );
    }

    #[test]
    fn field_rules_and_paths() {
        assert!(IcalField::Dtend.allows("endInclusive"));
        assert!(!IcalField::Dtstart.allows("endInclusive"));
        assert!(IcalField::Uid.remaps().is_empty());
        assert!(is_path("address.addressLocality"));
        assert!(!is_path("a.b.c.d") && !is_path("name|upper") && !is_path(""));
        assert!(!is_plain("a\nb", 10) && is_plain(", ", 10));
        let gaps = field_shape_gaps(
            IcalField::Dtstart,
            &FieldRemap {
                attribute: Some("startDate".into()),
                timezone: Some("Europe/Bratislava; rm".into()),
                ..FieldRemap::default()
            },
        );
        assert_eq!(gaps.len(), 1, "{gaps:?}");
    }
}
