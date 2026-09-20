//! What the data behind an endpoint contains, narrowed to the grant (T-0162, EP-46…EP-52).
//!
//! The gateway is not a compiler. `DataModel.spec.artifacts` names the files Model Tools
//! generated beside the LinkML source and committed in the same commit (DM-02), so what
//! an endpoint publishes is exactly what was reviewed. A model whose artifacts are not in
//! the checkout is still described: the JSON Schema and the `@context` are derived from
//! the grants, which is a narrower answer than the compiled one but never a wrong one.
//!
//! Whichever half answers, the document is a projection of the policy set: a class or a slot the
//! caller may not read is absent, and the index says only *that* something was left out —
//! `redacted: true`, no names, no count (EP-47, T-2133). A list of what is hidden publishes what was
//! being kept back: that the attribute exists, on which type, and that somebody thought it worth
//! hiding. A caller for whom nothing was left out gets no such key at all.

use super::formalisms;
use crate::pdp::evaluator::{
    effective, granted_attrs, granted_types, narrow, narrow_to_identity, Subject,
};
use crate::resolver::{Endpoint, Model};
use chrono::{DateTime, Utc};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet};

/// The media type of a JSON Schema document (EP-49).
pub const JSON_SCHEMA: &str = "application/schema+json";
/// The media type of a JSON-LD `@context` (EP-49).
pub const JSON_LD: &str = "application/ld+json";

/// The NGSI-LD core context every derived `@context` starts from, so a client that
/// expands a derived document still resolves `id`, `type` and the core terms.
const CORE_CONTEXT: &str = "https://uri.etsi.org/ngsi-ld/v1/ngsi-ld-core-context-v1.8.jsonld";

/// Members of an NGSI-LD entity that are the format rather than the data: no grant lists
/// them and no projection may remove them, or the schema stops describing NGSI-LD.
const STRUCTURAL: &[&str] = &[
    "id",
    "type",
    "@id",
    "@type",
    "@context",
    "scope",
    "createdAt",
    "modifiedAt",
    "deletedAt",
];

/// The media type of a Turtle document (EP-49).
pub const TURTLE: &str = "text/turtle";
/// The media type the OWL rendering is negotiated by, a Turtle profile (EP-49).
pub const TURTLE_OWL: &str = "text/turtle; profile=\"owl\"";
/// The media type of the LinkML source (EP-49).
pub const YAML: &str = "text/yaml";
/// The media type of the human documentation (EP-49).
pub const MARKDOWN: &str = "text/markdown";

/// Which schema document was asked for (EP-49).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Artifact {
    /// The JSON Schema draft-07 of the model, `model.schema.json` (DM-03).
    JsonSchema,
    /// The JSON-LD `@context` of the model (DM-05).
    Context,
    /// The SHACL shapes, rendered from the projection (T-0284).
    Shacl,
    /// The OWL ontology, rendered from the projection (T-0284).
    Owl,
    /// The RDFS rendering of the projection (T-0284).
    Rdf,
    /// The LinkML source of the projection (T-0284).
    LinkMl,
    /// The human documentation of the projection (T-0284).
    Markdown,
}

impl Artifact {
    /// The media type the artifact is served as.
    pub fn media_type(self) -> &'static str {
        match self {
            Artifact::JsonSchema => JSON_SCHEMA,
            Artifact::Context => JSON_LD,
            Artifact::Shacl | Artifact::Rdf => TURTLE,
            Artifact::Owl => TURTLE_OWL,
            Artifact::LinkMl => YAML,
            Artifact::Markdown => MARKDOWN,
        }
    }

    /// Whether the artifact is a JSON document rather than a rendered text one.
    pub fn is_json(self) -> bool {
        matches!(self, Artifact::JsonSchema | Artifact::Context)
    }

    /// The file name this artifact is published under (API/02 section 7a).
    pub fn file_name(self) -> &'static str {
        match self {
            Artifact::JsonSchema => "model.schema.json",
            Artifact::Context => "context.jsonld",
            Artifact::Shacl => "model.shacl.ttl",
            Artifact::Owl => "model.owl.ttl",
            Artifact::Rdf => "model.rdf.ttl",
            Artifact::LinkMl => "model.linkml.yaml",
            Artifact::Markdown => "model.md",
        }
    }

    /// The seven documents an endpoint publishes about one major (EP-46).
    pub const ALL: [Artifact; 7] = [
        Artifact::LinkMl,
        Artifact::JsonSchema,
        Artifact::Context,
        Artifact::Shacl,
        Artifact::Owl,
        Artifact::Rdf,
        Artifact::Markdown,
    ];
}

/// Names the artifact behind one path segment, or `None` when the surface has no such
/// document at all.
///
/// `model` is the negotiated name: `Accept` picks the formalism (EP-49), and the two
/// formalisms the gateway can produce itself are the JSON Schema and the `@context`.
pub fn artifact_of(segment: &str, accept: &str) -> Option<Artifact> {
    match segment {
        "json-schema" | "model.schema.json" => Some(Artifact::JsonSchema),
        "context.jsonld" | "context" => Some(Artifact::Context),
        "model.shacl.ttl" | "shacl" => Some(Artifact::Shacl),
        "model.owl.ttl" | "owl" => Some(Artifact::Owl),
        "model.rdf.ttl" | "rdf" => Some(Artifact::Rdf),
        "model.linkml.yaml" | "linkml" => Some(Artifact::LinkMl),
        "model.md" | "docs" => Some(Artifact::Markdown),
        "model" => Some(negotiated(accept)),
        _ => None,
    }
}

/// The formalism an `Accept` header asks `schema/v{major}/model` for (EP-49).
///
/// `text/turtle` alone is the SHACL, which is what a validator wants; the OWL is the same
/// media type with the `owl` profile, so the two share a name without sharing an answer.
fn negotiated(accept: &str) -> Artifact {
    if accept.contains("turtle") {
        if accept.contains("owl") {
            Artifact::Owl
        } else if accept.contains("rdf") {
            Artifact::Rdf
        } else {
            Artifact::Shacl
        }
    } else if accept.contains("yaml") {
        Artifact::LinkMl
    } else if accept.contains("markdown") {
        Artifact::Markdown
    } else if accept.contains("ld+json") {
        Artifact::Context
    } else {
        Artifact::JsonSchema
    }
}

/// One rendered formalism, as the caller receives it (T-0284, EP-47).
///
/// Every non-JSON formalism is built from the projected JSON Schema rather than from a
/// committed file, so a slot the projection removed cannot reappear in a Turtle document.
pub fn render(models: &[&Model], wanted: Artifact, visible: &Visible) -> String {
    let mut redacted = Vec::new();
    let schema = json_schema(models, visible, &mut redacted);
    let empty = Map::new();
    let defs = schema
        .get("$defs")
        .and_then(Value::as_object)
        .unwrap_or(&empty);
    let classes = formalisms::classes(models, defs);

    match wanted {
        Artifact::Shacl => formalisms::shacl(&classes),
        Artifact::Owl => formalisms::owl(&classes),
        Artifact::Rdf => formalisms::rdf(&classes),
        Artifact::LinkMl => formalisms::linkml(models, &classes),
        Artifact::Markdown => formalisms::markdown(models, &classes),
        // The two JSON documents have their own builders; this is not the way to them.
        Artifact::JsonSchema | Artifact::Context => String::new(),
    }
}

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
        if !self.covers_type(entity_type) {
            return None;
        }
        let mut narrowed = self.clone();
        narrowed.types = BTreeSet::from([entity_type.to_owned()]);
        Some(narrowed)
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
        STRUCTURAL.contains(&name)
            || (!self.denied_attrs.contains(name)
                && (self.attrs.is_empty() || self.attrs.contains(name)))
    }

    /// Whether the model may describe this attribute **on this class** (MP-02, EP-47).
    fn covers_slot(&self, class: &str, name: &str) -> bool {
        if !self.covers_attr(name) {
            return false;
        }
        match self.slots.get(class) {
            Some(slots) => STRUCTURAL.contains(&name) || slots.contains(name),
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

/// The catalogue of what this endpoint publishes (EP-46).
///
/// Only the artifacts the gateway can actually serve are listed, with the size and digest
/// of the projected document rather than of the file on disk: what a client fetches is
/// the projection, so that is what its `ETag` has to match.
pub fn index(endpoint: &Endpoint, visible: &Visible, digest: impl Fn(&[u8]) -> String) -> Value {
    let mut models = Vec::new();
    for model in &endpoint.models {
        let mut redacted = Vec::new();
        let schema = json_schema(std::slice::from_ref(&model), visible, &mut redacted);
        // The same list the JSON Schema fills: a term the committed `@context` carries and no
        // `$defs` mentions is redacted by this builder alone, and `redacted: true` has to be
        // raised from that too (EP-47, T-2341).
        let context = context(std::slice::from_ref(&model), visible, &mut redacted);

        let types: Vec<&String> = model
            .classes
            .iter()
            .filter(|class| visible.covers_type(class))
            .collect();
        // Whether anything was left out, and nothing about what: the names stay here (EP-47).
        let anything_redacted = !redacted.is_empty()
            || model
                .classes
                .iter()
                .any(|class| !visible.covers_type(class));

        let mut described = json!({
            "name": model.name,
            "version": model.major,
            "semver": model.version,
            "types": types,
            "artifacts": artifacts(std::slice::from_ref(&model), visible, &schema, &context, &digest),
        });
        if anything_redacted {
            described["redacted"] = json!(true);
        }
        models.push(described);
    }

    json!({ "endpoint": endpoint.slug, "models": models })
}

/// The seven documents one model publishes, each described by the projection a caller would
/// actually fetch rather than by the file on disk (EP-46, EP-48).
fn artifacts(
    models: &[&Model],
    visible: &Visible,
    schema: &Value,
    context: &Value,
    digest: &impl Fn(&[u8]) -> String,
) -> Value {
    let mut described = Map::new();
    for wanted in Artifact::ALL {
        let entry = match wanted {
            Artifact::JsonSchema => descriptor(schema, JSON_SCHEMA, digest),
            Artifact::Context => descriptor(context, JSON_LD, digest),
            other => text_descriptor(&render(models, other, visible), other.media_type(), digest),
        };
        described.insert(wanted.file_name().to_owned(), entry);
    }
    Value::Object(described)
}

/// One entry of the index's `artifacts` map.
fn descriptor(document: &Value, media_type: &str, digest: impl Fn(&[u8]) -> String) -> Value {
    let body = serde_json::to_vec(document).unwrap_or_default();
    json!({
        "type": media_type,
        "bytes": body.len(),
        "sha256": digest(&body),
    })
}

/// The same, for a rendered text document: the digest is of the bytes the caller receives, so
/// the index and the `ETag` of the document agree.
fn text_descriptor(body: &str, media_type: &str, digest: &impl Fn(&[u8]) -> String) -> Value {
    json!({
        "type": media_type,
        "bytes": body.len(),
        "sha256": digest(body.as_bytes()),
    })
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

/// The JSON-LD `@context` of every model of one major, projected to the grant (DM-05).
pub fn context(models: &[&Model], visible: &Visible, redacted: &mut Vec<String>) -> Value {
    let mut terms = Map::new();
    let mut derived = false;
    // One document, several models: a term one model calls a class and another an attribute,
    // or one derives and another has committed, is kept when any model covers it. What it is
    // reported as is therefore decided once, against the finished document.
    let mut left_out: Vec<String> = Vec::new();
    for model in models {
        match model.context.as_ref().and_then(inner_context) {
            Some(compiled) => {
                for (term, definition) in compiled {
                    // A term is either a class of this model or an attribute, and the two are
                    // not asked the same question. Answering both with one `||` chain meant a
                    // caller with no type whitelist — a grant that names no `information`, the
                    // dev seed's `public-read` among them — covered every name by the class
                    // test, and the attribute test in front of it could never refuse anything.
                    // Both things it protects were lost that way: the endpoint's
                    // `hiddenAttributes` (EP-61) and the attributes a prohibition takes back
                    // (GW8), because both live in `denied_attrs` (T-2341).
                    let covered = term.starts_with('@')
                        || if model.classes.iter().any(|class| class == term) {
                            visible.covers_type(term)
                        } else {
                            visible.covers_attr(term)
                        };
                    if covered {
                        terms.insert(term.clone(), definition.clone());
                    } else {
                        left_out.push(term.clone());
                    }
                }
            }
            None => {
                derived = true;
                for class in model.classes.iter().filter(|c| visible.covers_type(c)) {
                    terms.insert(class.clone(), json!(format!("#{class}")));
                }
                for attr in model
                    .classes
                    .iter()
                    .flat_map(|class| visible.slots_of(class))
                {
                    terms.insert(attr.clone(), json!(format!("#{attr}")));
                }
            }
        }
    }

    // A derived context has no vocabulary IRI of its own to point at, and inventing an
    // external one would be claiming an ontology nobody published: the terms resolve
    // against the document that carries them.
    if derived && !terms.contains_key("@vocab") {
        terms.insert("@vocab".to_owned(), json!("#"));
    }
    redacted.extend(
        left_out
            .into_iter()
            .filter(|term| !terms.contains_key(term)),
    );
    json!({ "@context": [CORE_CONTEXT, terms] })
}

/// The term map of a committed `@context`, whether the file is the map itself or the
/// usual `{"@context": {…}}` wrapper.
fn inner_context(document: &Value) -> Option<&Map<String, Value>> {
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
            slug: "s".to_owned(),
            title: std::collections::BTreeMap::new(),
            description: std::collections::BTreeMap::new(),
            space: "ovzdusie".to_owned(),
            project: "ovzdusie".to_owned(),
            audience: Audience::Public,
            allowed_projects: Vec::new(),
            representations: vec![Representation::NgsiLd],
            rate_limit: None,
            file_limits: None,
            hidden_attributes: Default::default(),
            projection: None,
            view_mapping: None,
            base_path: "/api/endpoint/s".to_owned(),
            models: vec![model(&["AirQualityObserved", "Vehicle"])],
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
