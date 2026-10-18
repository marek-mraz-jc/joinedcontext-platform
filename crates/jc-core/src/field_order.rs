//! The importance order and the readable names of NGSI-LD attributes (T-4661, T-4650).
//!
//! One table, `data/field-order.json`, says in which order a person reads an entity's
//! attributes (name, main time, values, category, place, link, the rest, the record's
//! metadata, coordinates, identifier) and what each is called in a language. The CKAN
//! publisher orders and labels DataStore fields from it, the assistant widget labels its table
//! columns from it, and the CKAN theme keeps a byte copy for the map popup, so the three never
//! drift. The enumeration names in the table are the theme's alone (the map popup).

use std::collections::BTreeMap;
use std::sync::LazyLock;

use serde::Deserialize;

/// The table itself, for a page that reads it in the browser.
pub const JSON: &str = include_str!("../data/field-order.json");

/// The group an attribute no group lists falls in when it holds a number.
const NUMBERS: &str = "value";
/// The group an attribute no group lists falls in otherwise.
const REST: &str = "other";

#[derive(Deserialize)]
struct Table {
    groups: Vec<Group>,
    labels: BTreeMap<String, BTreeMap<String, String>>,
}

#[derive(Deserialize)]
struct Group {
    group: String,
    attributes: Vec<String>,
}

static TABLE: LazyLock<Table> =
    LazyLock::new(|| serde_json::from_str(JSON).expect("data/field-order.json is valid"));

/// The attribute a flattened column belongs to: `name` of `name.languageMap.sk`, `location`
/// of `location.value.coordinates[0]`.
pub fn attribute(column: &str) -> &str {
    column.split(['.', '[']).next().unwrap_or(column)
}

/// Ranks per group: an attribute's rank is its group's index times this plus its place in the
/// group's list, an attribute the list does not name taking the last place.
const PER_GROUP: usize = 1000;

/// Where a column stands in the reading order, smaller first: by group, then in the order the
/// group lists its attributes (`startDate` before `startTime`). `numeric` says whether the column
/// holds numbers, which puts an attribute the table does not list among the values.
pub fn rank(column: &str, numeric: bool) -> usize {
    let attribute = attribute(column);
    let listed = TABLE.groups.iter().enumerate().find_map(|(index, g)| {
        let at = g.attributes.iter().position(|a| a == attribute)?;
        Some(index * PER_GROUP + at)
    });
    listed.unwrap_or_else(|| {
        let fallback = if numeric { NUMBERS } else { REST };
        let index = TABLE.groups.iter().position(|g| g.group == fallback);
        index.map_or(usize::MAX, |index| index * PER_GROUP + PER_GROUP - 1)
    })
}

/// The name of the group a column falls in (`name`, `time`, `value`, … `location`, `id`).
pub fn group(column: &str, numeric: bool) -> &'static str {
    TABLE
        .groups
        .get(rank(column, numeric) / PER_GROUP)
        .map_or(REST, |g| g.group.as_str())
}

/// What a person calls a column in `lang`, else in English: the label of its innermost named
/// part (`streetAddress` of `address.value.streetAddress`), else of its attribute, else the
/// name spelt as words (`eventCategory` → "Event category"). Coordinates read as longitude,
/// latitude and geometry type.
pub fn label(column: &str, lang: &str) -> String {
    let attribute = attribute(column);
    let leaf = if column.ends_with(".coordinates[0]") {
        "longitude"
    } else if column.ends_with(".coordinates[1]") {
        "latitude"
    } else if attribute == "location" && column.ends_with(".value.type") {
        "geometryType"
    } else {
        leaf(column)
    };
    word(leaf, lang)
        .or_else(|| word(attribute, lang))
        .unwrap_or_else(|| spelt(leaf))
}

fn word(key: &str, lang: &str) -> Option<String> {
    in_lang(TABLE.labels.get(key), lang)
}

fn in_lang(texts: Option<&BTreeMap<String, String>>, lang: &str) -> Option<String> {
    let texts = texts?;
    texts.get(lang).or_else(|| texts.get("en")).cloned()
}

/// The innermost named part of a flattened column, without `value`, `object`, `languageMap`,
/// the language after it and array indexes.
pub fn leaf(column: &str) -> &str {
    let mut parts = column.split('.').map(|p| p.split('[').next().unwrap_or(p));
    let mut found = attribute(column);
    while let Some(part) = parts.next() {
        match part {
            "value" | "object" | "" => {}
            "languageMap" => {
                parts.next();
            }
            named => found = named,
        }
    }
    found
}

/// `eventCategory` as "Event category", `air_quality` as "Air quality".
fn spelt(name: &str) -> String {
    let mut out = String::new();
    for (i, c) in name.chars().enumerate() {
        if c == '_' || c == '-' {
            out.push(' ');
        } else if i == 0 {
            out.extend(c.to_uppercase());
        } else if c.is_uppercase() {
            out.push(' ');
            out.extend(c.to_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_table_parses_and_every_group_attribute_has_a_label() {
        for g in &TABLE.groups {
            for a in &g.attributes {
                assert!(TABLE.labels.contains_key(a), "{a} has no label");
            }
        }
    }

    /// The CKAN theme serves a byte copy (joinedcontext-deployment
    /// charts/ckan/files/jc_theme/public__jc-field-order.json, whose test pins the same hash): a
    /// change here lands there too, with both hashes.
    #[test]
    fn the_table_is_the_one_the_ckan_theme_copies() {
        use sha2::{Digest, Sha256};
        let hash: String = Sha256::digest(JSON.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(
            hash,
            "4e949e7dc19acbe9d53a689f88ef07805d018a26791f7d52f631b925fa21c325"
        );
    }

    #[test]
    fn name_comes_first_coordinates_and_id_last() {
        let mut columns = vec![
            "_id",
            "location.value.coordinates[0]",
            "location.value.type",
            "source.value",
            "url.value",
            "startTime.value",
            "eventCategory.value",
            "startDate.value",
            "name.languageMap.sk",
        ];
        columns.sort_by_key(|c| rank(c, false));
        assert_eq!(
            columns,
            [
                "name.languageMap.sk",
                "startDate.value",
                "startTime.value",
                "eventCategory.value",
                "url.value",
                "source.value",
                "location.value.coordinates[0]",
                "location.value.type",
                "_id",
            ]
        );
    }

    #[test]
    fn an_unlisted_number_is_a_value_and_an_unlisted_text_the_rest() {
        assert_eq!(group("benzene.value", true), "value");
        assert_eq!(group("remark.value", false), "other");
        assert!(rank("no2.value", false) < rank("eventCategory.value", false));
    }

    #[test]
    fn labels_come_from_the_table_else_the_name_spelt() {
        assert_eq!(label("name.languageMap.sk", "sk"), "Názov");
        assert_eq!(label("startDate.value", "sk"), "Začiatok");
        assert_eq!(label("url.value", "cs"), "Odkaz");
        assert_eq!(label("address.value.streetAddress", "sk"), "Ulica");
        assert_eq!(
            label("location.value.coordinates[1]", "sk"),
            "Zemepisná šírka"
        );
        assert_eq!(label("location.value.type", "en"), "Geometry type");
        assert_eq!(label("location.geojson", "sk"), "Poloha");
        assert_eq!(label("startDate.value", "fi"), "Start");
        assert_eq!(label("eventSubCategory.value", "sk"), "Event sub category");
        assert_eq!(label("tags.value[0]", "en"), "Tags");
    }
}
