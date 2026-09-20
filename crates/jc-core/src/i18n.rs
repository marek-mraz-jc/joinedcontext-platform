//! Multi-language text map and resolution rules (PF-24..PF-26, PF-28).

use crate::error::{Error, Result};
use crate::names;
use std::collections::BTreeMap;

/// Multi-language text map keyed by ISO 639-1 lowercase two-letter language codes (PF-24).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MultiLanguageMap(BTreeMap<String, String>);

impl MultiLanguageMap {
    /// Creates an empty multi-language map.
    pub fn new() -> Self {
        Self(BTreeMap::new())
    }

    /// Returns the localized string for `locale` if present.
    pub fn get(&self, locale: &str) -> Option<&str> {
        self.0.get(locale).map(|s| s.as_str())
    }

    /// Inserts a localized string after validating the locale code (PF-25).
    pub fn insert(&mut self, locale: &str, text: impl Into<String>) -> Result<()> {
        names::validate_locale(locale)?;
        self.0.insert(locale.to_string(), text.into());
        Ok(())
    }

    /// Resolves localized text according to client preferences and fallback locale (PF-28).
    ///
    /// Resolution order:
    /// 1. First entry of `preferred` that exists in the map
    /// 2. `fallback` locale if present
    /// 3. First entry in deterministic [`BTreeMap`] order
    /// 4. Empty string if map is empty
    pub fn resolve(&self, preferred: &[String], fallback: &str) -> &str {
        for pref in preferred {
            if let Some(val) = self.get(pref) {
                return val;
            }
        }
        if let Some(val) = self.get(fallback) {
            return val;
        }
        if let Some((_, val)) = self.0.iter().next() {
            return val.as_str();
        }
        ""
    }

    /// Validates that the organization's default fallback locale is present (PF-26).
    pub fn require_fallback(&self, fallback: &str) -> Result<()> {
        if self.get(fallback).is_some() {
            Ok(())
        } else {
            Err(Error::MissingFallbackLocale(fallback.to_string()))
        }
    }

    /// Returns `true` if the map contains no localized entries.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Returns the number of localized translations.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Returns an iterator over `(locale, text)` pairs in deterministic sorted order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }
}

impl TryFrom<BTreeMap<String, String>> for MultiLanguageMap {
    type Error = Error;

    fn try_from(map: BTreeMap<String, String>) -> Result<Self> {
        for key in map.keys() {
            names::validate_locale(key)?;
        }
        Ok(Self(map))
    }
}

impl serde::Serialize for MultiLanguageMap {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.0.serialize(serializer)
    }
}

impl<'de> serde::Deserialize<'de> for MultiLanguageMap {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let map = BTreeMap::<String, String>::deserialize(deserializer)?;
        Self::try_from(map).map_err(serde::de::Error::custom)
    }
}

impl schemars::JsonSchema for MultiLanguageMap {
    fn schema_name() -> String {
        "MultiLanguageMap".to_string()
    }

    fn json_schema(gen: &mut schemars::gen::SchemaGenerator) -> schemars::schema::Schema {
        let value_schema = <String as schemars::JsonSchema>::json_schema(gen);
        let schema = schemars::schema::SchemaObject {
            instance_type: Some(schemars::schema::InstanceType::Object.into()),
            object: Some(Box::new(schemars::schema::ObjectValidation {
                additional_properties: Some(Box::new(value_schema)),
                property_names: Some(Box::new(schemars::schema::Schema::Object(
                    schemars::schema::SchemaObject {
                        instance_type: Some(schemars::schema::InstanceType::String.into()),
                        string: Some(Box::new(schemars::schema::StringValidation {
                            pattern: Some("^[a-z]{2}$".to_string()),
                            ..Default::default()
                        })),
                        ..Default::default()
                    },
                ))),
                ..Default::default()
            })),
            metadata: Some(Box::new(schemars::schema::Metadata {
                description: Some(
                    "Multi-language map keyed by ISO 639-1 two-letter lowercase language codes"
                        .to_string(),
                ),
                ..Default::default()
            })),
            ..Default::default()
        };
        schemars::schema::Schema::Object(schema)
    }
}

/// A human label or description (UI-50): one plain string in the author's language. The legacy
/// language map is still read, through release `v0.9`, and resolves to one string.
#[derive(
    Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(untagged)]
pub enum Text {
    /// The form every new manifest writes.
    Plain(String),
    /// The legacy `{locale: text}` map.
    Localized(MultiLanguageMap),
}

impl Text {
    /// The text for `locale` in the legacy map; a plain string belongs to no locale.
    pub fn get(&self, locale: &str) -> Option<&str> {
        match self {
            Self::Plain(_) => None,
            Self::Localized(map) => map.get(locale),
        }
    }

    /// The one string to show: the plain text, or the legacy map resolved as PF-28 orders it.
    pub fn resolve(&self, preferred: &[String], fallback: &str) -> &str {
        match self {
            Self::Plain(text) => text,
            Self::Localized(map) => map.resolve(preferred, fallback),
        }
    }
}

impl From<&str> for Text {
    fn from(text: &str) -> Self {
        Self::Plain(text.to_owned())
    }
}
