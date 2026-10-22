//! What one caller may read of an endpoint's models, as the gateway's own policy evaluator sees
//! it: the types and attributes in force, and the JSON Schema projected to them (EP-47).
//!
//! The schema documents themselves are the broker's LinkML surface of the Endpoint's view
//! (plan 5.4, T-3645, T-3646); nothing here renders one for a caller. What is left serves the
//! NGSI-LD entry document, the OpenAPI document, the MCP hub's type list and the projection's
//! `@context` reading, all of them map 7.1 code that T-3644 deletes with the evaluator.
//!
//! A class or a slot the caller may not read is absent, and nothing says which (EP-47, T-2133).

use crate::pdp::evaluator::{
    effective, granted_attrs, granted_types, narrow, narrow_to_identity, Subject,
};
use crate::resolver::{Endpoint, Model};
use chrono::{DateTime, Utc};
use jc_core::system_members::is_entity_system_member;
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet};

/// What one caller may read here: the union of the permissions in force, less whatever a
/// prohibition takes back (GW8, EP-47).
#[derive(Debug, Clone, Default)]
pub struct Visible {
    /// The entity types the caller may read; empty means every type the model declares.
    types: BTreeSet<String>,
    /// The attributes the caller may read; empty means every attribute of those types.
    attrs: BTreeSet<String>,
    /// Types no permission reaches any more because a prohibition covers them whole.
    denied_types: BTreeSet<String>,
    /// Attributes a prohibition takes back.
    denied_attrs: BTreeSet<String>,
    /// The slots the projection gives each class, when the endpoint projects a model.
    ///
    /// A grant's attribute list is one set for every type it names, but a projection is not:
    /// it says which slots each class has. Held per class, so a caller granted `age` on `User`
    /// is never described an `age` on `Vehicle` (MP-02, T-2132).
    slots: BTreeMap<String, BTreeSet<String>>,
}

impl Visible {
    /// The same surface narrowed to one entity type, or `None` when the caller may not read it
    /// (EP-47): a type nobody granted is refused exactly as an unknown one is.
    pub fn only(&self, entity_type: &str) -> Option<Visible> {
        self.only_each([entity_type]).ok()
    }

    /// The same narrowed to several entity types at once, so an agent loads the two classes it
    /// needs in one read rather than one document per class (EP-47).
    ///
    /// The first name the caller may not read is given back instead of a narrowed surface, and
    /// a name nobody granted is that same refusal as an unknown one: a list cannot be used to
    /// ask which of its names exists (SP-20).
    pub fn only_each<'a>(
        &self,
        wanted: impl IntoIterator<Item = &'a str>,
    ) -> Result<Visible, &'a str> {
        let mut types = BTreeSet::new();
        for entity_type in wanted {
            if !self.covers_type(entity_type) {
                return Err(entity_type);
            }
            types.insert(entity_type.to_owned());
        }
        let mut narrowed = self.clone();
        narrowed.types = types;
        Ok(narrowed)
    }

    /// Whether the model may describe this entity type at all.
    fn covers_type(&self, name: &str) -> bool {
        !self.denied_types.contains(name) && (self.types.is_empty() || self.types.contains(name))
    }

    /// Whether the model may describe this attribute at all, on any class it belongs to.
    ///
    /// The `@context` is one document for the whole model, so a term is in it when any class
    /// the caller reads carries it; a class definition is narrowed by [`Self::covers_slot`].
    fn covers_attr(&self, name: &str) -> bool {
        is_entity_system_member(name)
            || (!self.denied_attrs.contains(name)
                && (self.attrs.is_empty() || self.attrs.contains(name)))
    }

    /// Whether the model may describe this attribute **on this class** (MP-02, EP-47).
    fn covers_slot(&self, class: &str, name: &str) -> bool {
        if !self.covers_attr(name) {
            return false;
        }
        match self.slots.get(class) {
            Some(slots) => is_entity_system_member(name) || slots.contains(name),
            // No projection: the grant's own attribute set is the whole of the narrowing, as
            // it was before a projection could disagree with it.
            None => true,
        }
    }

    /// The attributes of one class the caller may be described, in the order they are rendered.
    fn slots_of<'a>(&'a self, class: &'a str) -> impl Iterator<Item = &'a String> + 'a {
        self.attrs
            .iter()
            .filter(move |attr| self.covers_slot(class, attr))
    }
}

/// The caller's visible surface, from the same policy set the data paths enforce.
pub fn visible(subject: &Subject, endpoint: &Endpoint, now: DateTime<Utc>) -> Visible {
    let (prohibitions, grants): (Vec<_>, Vec<_>) = effective(subject, &endpoint.policies, now)
        .into_iter()
        .partition(|policy| policy.effect.is_prohibition());

    let mut visible = Visible::default();
    for policy in &grants {
        visible.types.extend(granted_types(&policy.information));
        visible.attrs.extend(granted_attrs(&policy.information));
    }
    for policy in &prohibitions {
        let attrs = granted_attrs(&policy.information);
        // A prohibition that names attributes takes those back; one that names none takes
        // the whole type back.
        if attrs.is_empty() {
            visible
                .denied_types
                .extend(granted_types(&policy.information));
        } else {
            visible.denied_attrs.extend(attrs);
        }
    }
    // EP-61: what the endpoint does not serve is not described either, so the schema and
    // the data cannot disagree about which attributes exist.
    visible
        .denied_attrs
        .extend(endpoint.hidden_attributes.iter().cloned());
    // MP-03: the projection's classes and slots, intersected the same way, so the served
    // model is the projected model and its ETag moves when the projection does.
    if let Some(projection) = &endpoint.projection {
        let classes: BTreeSet<String> = projection
            .classes
            .iter()
            .map(|class| class.name.clone())
            .collect();
        visible.types = narrow(&visible.types, &classes);
        let slots: BTreeSet<String> = projection
            .classes
            .iter()
            .filter_map(|class| projection.attributes_of(&class.name))
            .flatten()
            .collect();
        visible.attrs = narrow_to_identity(&visible.attrs, &slots);
        visible.slots = projection
            .classes
            .iter()
            .filter_map(|class| {
                projection
                    .attributes_of(&class.name)
                    .map(|slots| (class.name.clone(), slots))
            })
            .collect();
    }
    visible
}

/// The entity types the caller may see described, across every model the endpoint
/// publishes (EP-47).
///
/// Sorted and deduplicated: two model versions declare the same class, and a list that
/// named it twice would suggest two different things exist.
pub fn visible_types(endpoint: &Endpoint, visible: &Visible) -> Vec<String> {
    let mut types: BTreeSet<&str> = BTreeSet::new();
    for model in &endpoint.models {
        for class in &model.classes {
            if visible.covers_type(class) {
                types.insert(class.as_str());
            }
        }
    }
    types.into_iter().map(str::to_owned).collect()
}

/// The JSON Schema of every model of one major, projected to the grant (EP-47, DM-03).
///
/// A model whose artifact the checkout carries is projected; one whose artifact is
/// missing is derived from the grant, so the two halves of a mixed major answer in one
/// document rather than one of them answering nothing.
pub fn json_schema(models: &[&Model], visible: &Visible, redacted: &mut Vec<String>) -> Value {
    let mut defs = Map::new();
    let mut title = Vec::new();
    for model in models {
        title.push(format!("{} {}", model.name, model.version));
        match &model.json_schema {
            Some(compiled) => project_defs(compiled, model, visible, redacted, &mut defs),
            None => derive_defs(model, visible, &mut defs),
        }
    }

    json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "title": title.join(", "),
        "$defs": defs,
    })
}

/// Copies the class definitions of a compiled schema across, without the classes and the
/// slots the grant does not reach.
fn project_defs(
    compiled: &Value,
    model: &Model,
    visible: &Visible,
    redacted: &mut Vec<String>,
    into: &mut Map<String, Value>,
) {
    // LinkML writes classes into `$defs`, older generators into `definitions`; anything
    // that is neither is left where it is, because a `$ref` may point at it.
    let sources = ["$defs", "definitions"]
        .into_iter()
        .filter_map(|key| compiled.get(key).and_then(Value::as_object));

    for definitions in sources {
        for (name, definition) in definitions {
            // A definition that is not one of the model's own classes is a shared type or
            // an enum: removing it would break the `$ref` of a class that survives.
            if model.classes.contains(name) && !visible.covers_type(name) {
                redacted.push(name.clone());
                continue;
            }
            let mut definition = definition.clone();
            redact_slots(&mut definition, name, visible, redacted);
            into.insert(name.clone(), definition);
        }
    }

    // A compiled schema that describes one class inline, without a `$defs` map at all.
    if into.is_empty() && compiled.get("properties").is_some() {
        for class in model.classes.iter().filter(|c| visible.covers_type(c)) {
            let mut definition = compiled.clone();
            redact_slots(&mut definition, class, visible, redacted);
            into.insert(class.clone(), definition);
        }
    }
}

/// Removes the properties the grant does not reach, and every mention of them.
fn redact_slots(
    definition: &mut Value,
    class: &str,
    visible: &Visible,
    redacted: &mut Vec<String>,
) {
    let Some(properties) = definition
        .get_mut("properties")
        .and_then(Value::as_object_mut)
    else {
        return;
    };
    properties.retain(|name, _| {
        let kept = visible.covers_slot(class, name);
        if !kept {
            redacted.push(format!("{class}.{name}"));
        }
        kept
    });
    let surviving: BTreeSet<String> = properties.keys().cloned().collect();

    if let Some(Value::Array(required)) = definition.get_mut("required") {
        required.retain(|name| name.as_str().is_some_and(|name| surviving.contains(name)));
    }
    // A pattern could match a slot that was just removed, and a form generated from the
    // schema would then offer a field the caller may not read.
    if let Some(members) = definition.as_object_mut() {
        members.remove("patternProperties");
    }
}

/// The description of a model the checkout has no artifact for: the entity types the
/// grant names, carrying the attributes the grant names.
fn derive_defs(model: &Model, visible: &Visible, into: &mut Map<String, Value>) {
    for class in model.classes.iter().filter(|c| visible.covers_type(c)) {
        let mut properties = Map::new();
        properties.insert(
            "id".to_owned(),
            json!({ "type": "string", "format": "uri" }),
        );
        properties.insert("type".to_owned(), json!({ "const": class }));
        for attr in visible.slots_of(class) {
            // Without the compiled model there is no datatype to state, and stating one
            // the model never declared would be an invention.
            properties.insert(attr.clone(), json!({}));
        }

        into.insert(
            class.clone(),
            json!({
                "type": "object",
                "description": format!(
                    "derived from the endpoint's grants; {} has no committed artifacts",
                    model.name
                ),
                "properties": properties,
                "required": ["id", "type"],
                // The grant names what may be read, never what the model declares, so the
                // derived schema cannot claim to be closed.
                "additionalProperties": true,
            }),
        );
    }
}

/// The term map of a committed `@context`, whether the file is the map itself or the
/// usual `{"@context": {…}}` wrapper.
pub(crate) fn inner_context(document: &Value) -> Option<&Map<String, Value>> {
    match document.get("@context") {
        Some(Value::Object(terms)) => Some(terms),
        Some(Value::Array(parts)) => parts.iter().rev().find_map(Value::as_object),
        _ => document.as_object(),
    }
}

#[cfg(test)]
mod visible_tests {
    use super::*;
    use crate::resolver::Model;
    use jc_core::kinds::{Audience, PolicySpec, Representation};

    fn policy(yaml: &str) -> PolicySpec {
        serde_norway::from_str(yaml).expect("the policy parses")
    }

    fn model(classes: &[&str]) -> Model {
        Model {
            name: "air-quality".to_owned(),
            version: "1.0.0".to_owned(),
            major: 1,
            classes: classes.iter().map(|class| (*class).to_owned()).collect(),
            json_schema: None,
            context: None,
        }
    }

    fn endpoint(policies: Vec<PolicySpec>) -> Endpoint {
        Endpoint {
            declared_types: None,
            roles: Default::default(),
            slug: "s".to_owned(),
            title: std::collections::BTreeMap::new(),
            description: std::collections::BTreeMap::new(),
            space: "ovzdusie".to_owned(),
            project: "ovzdusie".to_owned(),
            audience: Audience::Public,
            allowed_projects: Vec::new(),
            representations: vec![Representation::NgsiLd],
            rate_limit: None,
            creates: None,
            file_limits: None,
            hidden_attributes: Default::default(),
            projection: None,
            view_mapping: None,
            catalog: None,
            deprecation: None,
            feeds: Vec::new(),
            base_path: "/api/endpoint/s".to_owned(),
            models: vec![model(&["AirQualityObserved", "Vehicle"])],
            policy_names: Vec::new(),
            manifest_name: String::new(),
            policies,
        }
    }

    fn granting(types: &[&str], attrs: &[&str]) -> PolicySpec {
        let entities = types
            .iter()
            .map(|entity_type| format!("      - type: {entity_type}"))
            .collect::<Vec<_>>()
            .join("\n");
        policy(&format!(
            "contextSpaceRef: ovzdusie\n\
             assigner: did:web:hel.fi\n\
             assignee: {{ kind: role, id: public }}\n\
             operations: [queryEntity]\n\
             information:\n  - entities:\n{entities}\n    propertyNames: [{}]\n",
            attrs.join(", ")
        ))
    }

    fn prohibiting(types: &[&str], attrs: &[&str]) -> PolicySpec {
        let mut spec = granting(types, attrs);
        spec.effect = jc_core::kinds::PolicyEffect::Prohibition;
        spec
    }

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-09-18T00:00:00Z")
            .expect("a fixed instant")
            .with_timezone(&Utc)
    }

    /// T-1158, GW8, EP-47: a prohibition that names no attribute takes the whole type back, and
    /// one that names attributes takes only those.
    #[test]
    fn a_prohibition_takes_back_a_type_or_the_attributes_it_names() {
        let whole_type = endpoint(vec![
            granting(&["AirQualityObserved", "Vehicle"], &["pm10", "speed"]),
            prohibiting(&["Vehicle"], &[]),
        ]);
        let seen = visible(&Subject::anonymous(), &whole_type, now());
        assert!(seen.covers_type("AirQualityObserved"));
        assert!(
            !seen.covers_type("Vehicle"),
            "the prohibition took the type"
        );
        assert_eq!(
            visible_types(&whole_type, &seen),
            vec!["AirQualityObserved"]
        );

        let one_attribute = endpoint(vec![
            granting(&["AirQualityObserved"], &["pm10", "pm25"]),
            prohibiting(&["AirQualityObserved"], &["pm25"]),
        ]);
        let seen = visible(&Subject::anonymous(), &one_attribute, now());
        assert!(seen.covers_type("AirQualityObserved"), "the type stays");
        assert!(seen.covers_attr("pm10"));
        assert!(
            !seen.covers_attr("pm25"),
            "the prohibition took the attribute"
        );
    }

    /// EP-61: what the endpoint does not serve is not described either, whatever a grant says,
    /// so the schema and the data cannot disagree about which attributes exist.
    #[test]
    fn a_hidden_attribute_is_never_described_even_where_a_grant_names_it() {
        let mut hiding = endpoint(vec![granting(
            &["AirQualityObserved"],
            &["pm10", "contact"],
        )]);
        hiding.hidden_attributes = ["contact".to_owned()].into_iter().collect();

        let seen = visible(&Subject::anonymous(), &hiding, now());
        assert!(seen.covers_attr("pm10"));
        assert!(!seen.covers_attr("contact"));
    }

    /// The identity of an entity is not an attribute a grant lists: `id` and `type` are how it
    /// is addressed at all, so they survive a narrowing that names neither (EP-47).
    #[test]
    fn the_structural_members_survive_every_narrowing() {
        let narrow = endpoint(vec![granting(&["AirQualityObserved"], &["pm10"])]);
        let seen = visible(&Subject::anonymous(), &narrow, now());
        assert!(seen.covers_attr("id"));
        assert!(seen.covers_attr("type"));
        assert!(!seen.covers_attr("pm25"));
    }

    /// An endpoint with no policy at all describes what it publishes: an empty grant set means
    /// "every type the model declares", and the request reached this surface only because the
    /// PDP admitted it. The narrowing here is the schema's, not the door's.
    #[test]
    fn no_policy_describes_the_published_model_rather_than_nothing() {
        let bare = endpoint(Vec::new());
        let seen = visible(&Subject::anonymous(), &bare, now());
        assert!(seen.covers_type("AirQualityObserved"));
        assert!(seen.covers_attr("anything"));
        assert_eq!(visible_types(&bare, &seen).len(), 2);
    }

    /// A type nobody granted is refused exactly as an unknown one is (EP-47).
    #[test]
    fn only_answers_nothing_for_a_type_the_caller_may_not_read() {
        let granted = endpoint(vec![granting(&["AirQualityObserved"], &["pm10"])]);
        let seen = visible(&Subject::anonymous(), &granted, now());
        assert!(seen.only("AirQualityObserved").is_some());
        assert!(seen.only("Vehicle").is_none());
    }
}
