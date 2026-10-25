//! The order, key attributes and labels a person reads an entity's attributes in (DM-98…DM-103,
//! EP-135, T-4672).
//!
//! The DataModel's LinkML says it: `rank` on a slot or in the class's `slot_usage`, then the
//! class's own slots as written, then the ones it inherits; `annotations` `summary`, `label` and
//! `hidden`. An Endpoint's `spec.display` or a visualization's `fields` reorders and narrows that
//! order; [`field_order`] places what the model does not list, and labels what it does not name.
//! Everything is computed on the attributes the caller is served, so a slot the caller may not
//! read is never a column, a label or a key attribute (DM-102).

use std::collections::{BTreeMap, BTreeSet};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_norway::Value;

use crate::field_order;

/// The attributes shown last unless the model ranks them, in this order (DM-98).
const SYSTEM: &[&str] = &["location", "dateCreated", "dateModified", "type", "id"];

/// How many attributes are the key ones when the model marks none (DM-99).
const FIRST: usize = 4;

/// An override of the model's display order for one type: an Endpoint's `spec.display` entry or a
/// visualization's `fields` (EP-135). Either list empty leaves that part to the model.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Display {
    /// The slots shown, in this order; the rest are not shown.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<String>,
    /// The key attributes, in place of the model's `summary` annotations.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub summary: Vec<String>,
}

impl Display {
    /// Refuses a name that is no slot of the type in the projection (`slots`), naming every one
    /// at once, as `jcctl validate` and the Portal do (EP-135).
    pub fn check(&self, class: &str, slots: &BTreeSet<String>) -> Result<(), String> {
        let unknown: Vec<&str> = self
            .fields
            .iter()
            .chain(&self.summary)
            .filter(|name| !slots.contains(*name))
            .map(String::as_str)
            .collect();
        if unknown.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "{} is no slot of {class} in the Endpoint's projection (EP-135)",
                unknown.join(", ")
            ))
        }
    }
}

/// One attribute as it is shown: its place is its index in [`display_order`]'s answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Field {
    /// The attribute (slot) name.
    pub slot: String,
    /// One of the key attributes a popup, a card and a short answer show (DM-99).
    pub summary: bool,
    /// The model's labels per language.
    pub labels: BTreeMap<String, String>,
    /// The slot's `title`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The model's default language.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
}

impl Field {
    /// What a person calls the attribute in `lang` (DM-100): the model's label in `lang`, in the
    /// model's default language, in English; the slot's title; the heuristic's label; the name
    /// spelt as words. Text, never markup.
    pub fn label(&self, lang: &str) -> String {
        [Some(lang), self.language.as_deref(), Some("en")]
            .into_iter()
            .flatten()
            .find_map(|l| self.labels.get(l))
            .or(self.title.as_ref())
            .cloned()
            .unwrap_or_else(|| field_order::label(&self.slot, lang))
    }
}

/// What the model says about one slot of the class.
#[derive(Default)]
struct Slot {
    rank: Option<i64>,
    summary: bool,
    hidden: bool,
    labels: BTreeMap<String, String>,
    title: Option<String>,
    range: Option<String>,
    unit: Option<String>,
    multivalued: bool,
    /// The `ngsi_ld_kind` annotation: Property, Relationship, GeoProperty, LanguageProperty.
    kind: Option<String>,
}

/// The attributes of `class` the caller is served, in display order, each with its labels and
/// whether it is a key attribute (DM-98…DM-102). A type the model has no class for is ordered by
/// the heuristic and has key attributes only when `view` names them.
///
/// `linkml` is the DataModel's source (`None` for a type without one); `attributes` are the
/// attribute names the caller receives, each with whether it holds numbers (the fallback puts an
/// unlisted number among the values); `view` an override (EP-135). An override's name the caller
/// is not served is skipped here; refusing it is [`Display::check`]'s.
pub fn display_order(
    linkml: Option<&str>,
    class: &str,
    attributes: &[(&str, bool)],
    view: Option<&Display>,
) -> Result<Vec<Field>, String> {
    let schema: Option<Value> = linkml
        .map(serde_norway::from_str)
        .transpose()
        .map_err(|err| err.to_string())?;
    let language = schema
        .as_ref()
        .and_then(|s| s.get("in_language").or_else(|| s.get("default_language")))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let (written, slots) = match &schema {
        Some(schema) => model_slots(schema, class),
        None => (Vec::new(), BTreeMap::new()),
    };
    let served: BTreeMap<&str, bool> = attributes.iter().copied().collect();
    let key = |name: &str| {
        let slot = slots.get(name);
        let place = written.iter().position(|w| w == name);
        let fallback = field_order::rank(name, served.get(name).copied().unwrap_or(false));
        match (slot.and_then(|s| s.rank), place) {
            // A type without a model class is ordered by the heuristic alone (DM-98).
            _ if written.is_empty() => (2, 0, 0, fallback),
            (Some(rank), _) => (0, rank, place.unwrap_or(usize::MAX), fallback),
            _ if SYSTEM.contains(&name) => {
                (3, 0, SYSTEM.iter().position(|s| *s == name).unwrap_or(0), 0)
            }
            (None, Some(place)) => (1, 0, place, 0),
            (None, None) => (2, 0, 0, fallback),
        }
    };
    let mut order: Vec<&str> = served
        .keys()
        .copied()
        .filter(|name| !slots.get(*name).is_some_and(|s| s.hidden))
        .collect();
    order.sort_by_key(|name| key(name));
    if let Some(fields) = view.map(|v| &v.fields).filter(|f| !f.is_empty()) {
        order = fields
            .iter()
            .map(String::as_str)
            .filter(|name| order.contains(name))
            .collect();
    }
    let chosen: Vec<&str> = match view.map(|v| &v.summary).filter(|s| !s.is_empty()) {
        Some(summary) => summary.iter().map(String::as_str).collect(),
        None => order
            .iter()
            .copied()
            .filter(|name| slots.get(*name).is_some_and(|s| s.summary))
            .collect(),
    };
    let first: Vec<&str> = order.iter().copied().take(FIRST).collect();
    // A type without a model class has no key attributes of its own: its columns come and go
    // with the cells its rows fill, so "the first four" would move under its consumer.
    let summary = if chosen.iter().any(|name| order.contains(name)) {
        chosen
    } else if written.is_empty() {
        Vec::new()
    } else {
        first
    };
    Ok(order
        .into_iter()
        .map(|name| {
            let slot = slots.get(name);
            Field {
                slot: name.to_owned(),
                summary: summary.contains(&name),
                labels: slot.map(|s| s.labels.clone()).unwrap_or_default(),
                title: slot.and_then(|s| s.title.clone()),
                language: language.clone(),
            }
        })
        .collect())
}

/// The words each value of `class`'s `slot` reads as, per language (T-4682): the slot's range is
/// one of the model's `enums`, and each `permissible_values` entry's `label` annotation (a language
/// map) says it, its `description` in the model's language where the label has none. `None` when
/// the range is no enum of the model or no value says anything; a value without words is left out,
/// so it is shown as written. Text, never markup.
pub fn enum_words(
    linkml: &str,
    class: &str,
    slot: &str,
) -> Option<BTreeMap<String, BTreeMap<String, String>>> {
    let schema: Value = serde_norway::from_str(linkml).ok()?;
    let node = schema.get("classes").and_then(|c| c.get(class));
    let range = [
        node.and_then(|n| n.get("slot_usage"))
            .and_then(|u| u.get(slot)),
        node.and_then(|n| n.get("attributes"))
            .and_then(|a| a.get(slot)),
        schema.get("slots").and_then(|s| s.get(slot)),
    ]
    .into_iter()
    .flatten()
    .find_map(|n| n.get("range")?.as_str())?;
    let values = schema
        .get("enums")?
        .get(range)?
        .get("permissible_values")?
        .as_mapping()?;
    let language = schema
        .get("in_language")
        .or_else(|| schema.get("default_language"))
        .and_then(Value::as_str)
        .unwrap_or("en");
    let words: BTreeMap<String, BTreeMap<String, String>> = values
        .iter()
        .filter_map(|(value, about)| {
            let mut said: BTreeMap<String, String> = annotation(about, "label")
                .as_ref()
                .and_then(Value::as_mapping)
                .into_iter()
                .flatten()
                .filter_map(|(l, t)| Some((l.as_str()?.to_owned(), t.as_str()?.to_owned())))
                .collect();
            if let Some(text) = about.get("description").and_then(Value::as_str) {
                said.entry(language.to_owned())
                    .or_insert_with(|| text.to_owned());
            }
            let value = value.as_str()?.to_owned();
            (!said.is_empty()).then_some((value, said))
        })
        .collect();
    (!words.is_empty()).then_some(words)
}

/// One attribute as a query names it (T-4759).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryAttribute {
    /// The attribute (slot) name.
    pub slot: String,
    /// `attr` for text, `attr:range`, `attr:range[unit]` for a number in a UCUM unit, `attr:a|b`
    /// for the only allowed values, `attr:Type` for a Relationship, `:ref` for a link value, and
    /// `[]` after any of them for several values.
    pub written: String,
    /// A `q` on it: `attr>0` for a number, `attr=="a"` for the first allowed value; `None` for
    /// any other attribute.
    pub example: Option<String>,
}

/// The NGSI-LD members that qualify an attribute rather than being one (TS 104 175 4.5): a
/// model that writes them as slots still describes no attribute of that name (T-5642).
pub const SUB_PROPERTIES: &[&str] = &[
    "observedAt",
    "unitCode",
    "datasetId",
    "instanceId",
    "createdAt",
    "modifiedAt",
    "deletedAt",
    "lang",
];

/// The most allowed values one attribute lists; more end in `|…`.
const ENUM_VALUES: usize = 12;

/// The longest allowed value a card shows; a longer one is prose, not a value.
const VALUE_CHARS: usize = 64;

/// A class, slot, range or unit name a card may write: an NGSI-LD term or UCUM code, never prose.
/// The LinkML is the publisher's, and the card reaches a model as data (T-4779): a name with
/// spaces or line breaks could pose as a rule.
fn is_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 200
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_-:./#[]{}*^'%".contains(c))
}

/// An allowed value a card may list: one short line without the card's own `|`, `` ` `` or
/// angle brackets (T-4779).
fn is_value(value: &str) -> bool {
    let value = value.trim();
    !value.is_empty()
        && value.chars().count() <= VALUE_CHARS
        && !value
            .chars()
            .any(|c| c.is_control() || matches!(c, '|' | '`' | '<' | '>'))
}

/// Every class of the served LinkML with its attributes in written order, inherited ones
/// included, as [`QueryAttribute`]s (T-4759). The schema is the one the caller is served, so it
/// holds only what the caller may read; `id`, `type` and the [`SUB_PROPERTIES`] are not listed.
pub fn query_attributes(linkml: &str) -> Result<BTreeMap<String, Vec<QueryAttribute>>, String> {
    let schema: Value = serde_norway::from_str(linkml).map_err(|err| err.to_string())?;
    let enums = schema.get("enums");
    let allowed = |range: &str| -> Option<Vec<String>> {
        let values = enums?.get(range)?.get("permissible_values")?.as_mapping()?;
        Some(
            values
                .keys()
                .filter_map(|k| k.as_str().filter(|v| is_value(v)).map(str::to_owned))
                .collect::<Vec<_>>(),
        )
        .filter(|values| !values.is_empty())
    };
    let classes = schema.get("classes").and_then(Value::as_mapping);
    Ok(classes
        .into_iter()
        .flatten()
        .filter_map(|(name, _)| name.as_str())
        .filter(|class| is_name(class))
        .map(|class| {
            let (written, slots) = model_slots(&schema, class);
            let attributes = written
                .iter()
                .filter(|name| {
                    !matches!(name.as_str(), "id" | "type")
                        && !SUB_PROPERTIES.contains(&name.as_str())
                        && is_name(name)
                })
                .map(|name| {
                    let slot = &slots[name];
                    let range = slot.range.as_deref().unwrap_or("string");
                    let values = allowed(range);
                    let (mut text, example) = match (&values, slot.kind.as_deref()) {
                        (Some(values), _) => {
                            let mut shown = values[..values.len().min(ENUM_VALUES)].join("|");
                            if values.len() > ENUM_VALUES {
                                shown.push_str("|…");
                            }
                            let first = values.first().filter(|v| !v.contains(['"', '\\']));
                            (
                                format!("{name}:{shown}"),
                                first.map(|v| format!("{name}==\"{v}\"")),
                            )
                        }
                        (None, Some("GeoProperty")) => (format!("{name}:GeoJSON"), None),
                        (None, _) => match range {
                            "string" => (name.clone(), None),
                            "uriorcurie" | "uri" => (format!("{name}:ref"), None),
                            "integer" | "float" | "double" | "decimal" => {
                                let unit = slot
                                    .unit
                                    .as_ref()
                                    .filter(|u| is_name(u))
                                    .map(|u| format!("[{u}]"));
                                (
                                    format!("{name}:{range}{}", unit.unwrap_or_default()),
                                    Some(format!("{name}>0")),
                                )
                            }
                            other if is_name(other) => (format!("{name}:{other}"), None),
                            _ => (name.clone(), None),
                        },
                    };
                    if slot.multivalued {
                        text.push_str("[]");
                    }
                    QueryAttribute {
                        slot: name.clone(),
                        written: text,
                        example,
                    }
                })
                .collect();
            (class.to_owned(), attributes)
        })
        .collect())
}

/// The class's slots in written order (own `slots`, own `attributes`, then each ancestor's
/// through `is_a` and `mixins`, nearest first) and what the model says about each. A class the
/// model does not declare has none.
// ponytail: an ancestor's `slot_usage` is not read; the class itself or the global slot says it.
fn model_slots(schema: &Value, class: &str) -> (Vec<String>, BTreeMap<String, Slot>) {
    let classes = schema.get("classes");
    let mut written: Vec<String> = Vec::new();
    let mut seen = BTreeSet::new();
    let mut queue = vec![class.to_owned()];
    while !queue.is_empty() {
        let mut next = Vec::new();
        for name in queue {
            if !seen.insert(name.clone()) {
                continue;
            }
            let Some(node) = classes.and_then(|c| c.get(name.as_str())) else {
                continue;
            };
            let listed = node.get("slots").and_then(Value::as_sequence);
            let own = listed.into_iter().flatten().filter_map(Value::as_str);
            let attributes = node.get("attributes").and_then(Value::as_mapping);
            let keys = attributes
                .into_iter()
                .flatten()
                .filter_map(|(k, _)| k.as_str());
            for slot in own.chain(keys) {
                if !written.iter().any(|w| w == slot) {
                    written.push(slot.to_owned());
                }
            }
            next.extend(node.get("is_a").and_then(Value::as_str).map(str::to_owned));
            let mixins = node.get("mixins").and_then(Value::as_sequence);
            next.extend(
                mixins
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(str::to_owned),
            );
        }
        queue = next;
    }
    let node = classes.and_then(|c| c.get(class));
    let slots = written
        .iter()
        .map(|name| {
            let sources = [
                node.and_then(|n| n.get("slot_usage"))
                    .and_then(|u| u.get(name.as_str())),
                node.and_then(|n| n.get("attributes"))
                    .and_then(|a| a.get(name.as_str())),
                schema.get("slots").and_then(|s| s.get(name.as_str())),
            ];
            (name.clone(), slot(&sources))
        })
        .collect();
    (written, slots)
}

/// What the first of `sources` (most specific first) that says each thing says.
fn slot(sources: &[Option<&Value>]) -> Slot {
    let first =
        |read: &dyn Fn(&Value) -> Option<Value>| sources.iter().flatten().find_map(|n| read(n));
    let labels = first(&|n| annotation(n, "label").filter(Value::is_mapping));
    Slot {
        rank: first(&|n| n.get("rank").cloned()).and_then(|v| v.as_i64()),
        summary: first(&|n| annotation(n, "summary")).and_then(|v| v.as_bool()) == Some(true),
        hidden: first(&|n| annotation(n, "hidden")).and_then(|v| v.as_bool()) == Some(true),
        labels: labels
            .as_ref()
            .and_then(Value::as_mapping)
            .into_iter()
            .flatten()
            .filter_map(|(l, t)| Some((l.as_str()?.to_owned(), t.as_str()?.to_owned())))
            .collect(),
        title: first(&|n| n.get("title").cloned()).and_then(|v| v.as_str().map(str::to_owned)),
        range: first(&|n| n.get("range").cloned()).and_then(|v| v.as_str().map(str::to_owned)),
        unit: first(&|n| n.get("unit")?.get("ucum_code").cloned())
            .and_then(|v| v.as_str().map(str::to_owned)),
        multivalued: first(&|n| n.get("multivalued").cloned()).and_then(|v| v.as_bool())
            == Some(true),
        kind: first(&|n| annotation(n, "ngsi_ld_kind")).and_then(|v| v.as_str().map(str::to_owned)),
    }
}

/// An annotation's value, written plain (`summary: true`) or as LinkML's tag/value pair
/// (`summary: { tag: summary, value: true }`). The broker's schema surface serves every value as
/// text (`"true"`, a label's language map as JSON text), so a text is read back as YAML.
fn annotation(node: &Value, name: &str) -> Option<Value> {
    let value = node.get("annotations")?.get(name)?;
    let inner = match (value.get("tag"), value.get("value")) {
        (Some(_), Some(inner)) => inner,
        _ => value,
    };
    match inner.as_str() {
        Some(text) => Some(serde_norway::from_str(text).unwrap_or_else(|_| inner.clone())),
        None => Some(inner.clone()),
    }
}
