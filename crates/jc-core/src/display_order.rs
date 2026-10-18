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
}

/// The attributes of `class` the caller is served, in display order, each with its labels and
/// whether it is a key attribute (DM-98…DM-102).
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
    let summary = if chosen.iter().any(|name| order.contains(name)) {
        chosen
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
    }
}

/// An annotation's value, written plain (`summary: true`) or as LinkML's tag/value pair
/// (`summary: { tag: summary, value: true }`).
fn annotation(node: &Value, name: &str) -> Option<Value> {
    let value = node.get("annotations")?.get(name)?;
    match (value.get("tag"), value.get("value")) {
        (Some(_), Some(inner)) => Some(inner.clone()),
        _ => Some(value.clone()),
    }
}
