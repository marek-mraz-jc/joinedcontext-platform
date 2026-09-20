//! The five formalisms the gateway renders rather than serves (T-0284, EP-46, EP-47, EP-49).
//!
//! SHACL, OWL, RDF, the LinkML source and the Markdown are built from the projected model,
//! which is the class and slot set [`super::schema::json_schema`] already narrowed to the
//! caller's grant. That is the whole security argument of this module: a slot the projection
//! removed is absent from every formalism because every formalism is rendered from the
//! projection, so no document here can disagree with the JSON Schema, with the `@context` or
//! with what the data paths actually serve (EP-47).
//!
//! The gateway emits Turtle and never parses it. Redacting a triple out of a committed
//! document would need an RDF stack in a security path, and a parsed and re-serialised
//! document is no longer the reviewed bytes anyway, so the reason to serve the committed file
//! disappears the moment the file has to be narrowed. Architecture/04 section 1a records the
//! decision.

use crate::resolver::Model;
use serde_json::{Map, Value};
use std::fmt::Write as _;

/// The term namespace of one model version.
///
/// A URN rather than an `https://` IRI: the three RDF documents have to agree about what a
/// slot is, and minting a resolvable ontology address on the organization's behalf would
/// claim a document the platform does not publish.
fn vocabulary(model: &Model) -> String {
    format!("urn:joinedcontext:model:{}:v{}:", model.name, model.major)
}

/// One projected class: the entity type, the model that declares it, and the JSON Schema
/// definition that survived the projection.
pub struct Class<'a> {
    /// The NGSI-LD entity type.
    pub name: &'a str,
    /// The model the type belongs to, which decides its term namespace.
    pub model: &'a Model,
    /// The definition, already narrowed to the grant.
    pub definition: &'a Value,
}

impl Class<'_> {
    /// The absolute IRI of this class.
    fn iri(&self) -> String {
        format!("{}{}", vocabulary(self.model), self.name)
    }

    /// The absolute IRI of one of its slots.
    fn slot_iri(&self, slot: &str) -> String {
        format!("{}{}", vocabulary(self.model), slot)
    }

    /// The slots the caller may read, without the two members that are the entity's
    /// identity rather than its data.
    fn slots(&self) -> Vec<(&str, &Value)> {
        self.definition
            .get("properties")
            .and_then(Value::as_object)
            .map(|properties| {
                properties
                    .iter()
                    .filter(|(name, _)| name.as_str() != "id" && name.as_str() != "type")
                    .map(|(name, definition)| (name.as_str(), definition))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Whether the projected definition still requires this slot.
    fn requires(&self, slot: &str) -> bool {
        self.definition
            .get("required")
            .and_then(Value::as_array)
            .is_some_and(|names| names.iter().any(|name| name.as_str() == Some(slot)))
    }
}

/// Pairs every surviving definition with the model that declares it (EP-47).
///
/// A definition no model claims is a shared type or an enum the projection kept so a `$ref`
/// still resolves; it is not an entity type and no formalism renders it as one.
pub fn classes<'a>(models: &[&'a Model], defs: &'a Map<String, Value>) -> Vec<Class<'a>> {
    let mut classes = Vec::new();
    for (name, definition) in defs {
        if let Some(model) = models.iter().find(|m| m.classes.iter().any(|c| c == name)) {
            classes.push(Class {
                name,
                model,
                definition,
            });
        }
    }
    classes
}

/// The XSD datatype a projected slot carries, when the definition states one.
fn xsd(definition: &Value) -> Option<&'static str> {
    // An array states its member type in `items`; the datatype of the slot is that.
    let leaf = match definition.get("type").and_then(Value::as_str) {
        Some("array") => definition.get("items").unwrap_or(definition),
        _ => definition,
    };
    let format = leaf
        .get("format")
        .and_then(Value::as_str)
        .unwrap_or_default();
    match leaf.get("type").and_then(Value::as_str)? {
        "string" => Some(match format {
            "uri" | "iri" | "uri-reference" => "xsd:anyURI",
            "date-time" => "xsd:dateTime",
            "date" => "xsd:date",
            "time" => "xsd:time",
            _ => "xsd:string",
        }),
        "number" => Some("xsd:double"),
        "integer" => Some("xsd:integer"),
        "boolean" => Some("xsd:boolean"),
        _ => None,
    }
}

/// Whether the slot holds several values, which SHACL and LinkML both state explicitly.
fn multivalued(definition: &Value) -> bool {
    definition.get("type").and_then(Value::as_str) == Some("array")
}

/// The closed value set of a slot, when the projected definition states one.
fn enumeration(definition: &Value) -> Option<&Vec<Value>> {
    let definition = match definition.get("type").and_then(Value::as_str) {
        Some("array") => definition.get("items").unwrap_or(definition),
        _ => definition,
    };
    definition
        .get("enum")
        .and_then(Value::as_array)
        .filter(|values| !values.is_empty())
}

/// A JSON value as a Turtle literal, escaped so a description can hold a quote or a newline.
fn literal(value: &Value) -> String {
    match value {
        Value::String(text) => format!("\"{}\"", escape(text)),
        Value::Number(number) => number.to_string(),
        Value::Bool(flag) => flag.to_string(),
        other => format!("\"{}\"", escape(&other.to_string())),
    }
}

fn escape(text: &str) -> String {
    text.chars()
        .flat_map(|c| match c {
            '\\' => "\\\\".chars().collect::<Vec<_>>(),
            '"' => "\\\"".chars().collect(),
            '\n' => "\\n".chars().collect(),
            '\r' => "\\r".chars().collect(),
            '\t' => "\\t".chars().collect(),
            other => vec![other],
        })
        .collect()
}

/// A slot's one-line description, when the model states one.
fn description(definition: &Value) -> Option<&str> {
    definition.get("description").and_then(Value::as_str)
}

/// The header every rendered Turtle document carries.
fn turtle_header(what: &str) -> String {
    format!(
        "# {what}, rendered by the joinedcontext gateway from the model this endpoint grants.\n\
         # Classes and slots the caller may not read are absent (EP-47).\n\
         @prefix rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#> .\n\
         @prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .\n\
         @prefix xsd: <http://www.w3.org/2001/XMLSchema#> .\n"
    )
}

/// The SHACL shapes of the projected model (EP-46).
///
/// Never `sh:closed`. The entity behind a shape legitimately carries the slots this caller
/// may not read, and a closed shape would declare that entity invalid — the projection
/// narrows what is described, never what exists.
pub fn shacl(classes: &[Class]) -> String {
    let mut out = turtle_header("SHACL shapes");
    out.push_str("@prefix sh: <http://www.w3.org/ns/shacl#> .\n");

    for class in classes {
        let _ = write!(
            out,
            "\n<{}Shape>\n  a sh:NodeShape ;\n  sh:targetClass <{}> ;\n  sh:closed false",
            class.iri(),
            class.iri()
        );
        for (slot, definition) in class.slots() {
            let _ = write!(
                out,
                " ;\n  sh:property [\n    sh:path <{}> ;\n    sh:name \"{}\"",
                class.slot_iri(slot),
                escape(slot)
            );
            if let Some(datatype) = xsd(definition) {
                let _ = write!(out, " ;\n    sh:datatype {datatype}");
            }
            if let Some(values) = enumeration(definition) {
                let members: Vec<String> = values.iter().map(literal).collect();
                let _ = write!(out, " ;\n    sh:in ( {} )", members.join(" "));
            }
            if class.requires(slot) {
                out.push_str(" ;\n    sh:minCount 1");
            }
            if !multivalued(definition) {
                out.push_str(" ;\n    sh:maxCount 1");
            }
            if let Some(text) = description(definition) {
                let _ = write!(out, " ;\n    rdfs:comment \"{}\"", escape(text));
            }
            out.push_str("\n  ]");
        }
        out.push_str(" .\n");
    }
    out
}

/// The OWL ontology of the projected model (EP-46).
pub fn owl(classes: &[Class]) -> String {
    let mut out = turtle_header("OWL ontology");
    out.push_str("@prefix owl: <http://www.w3.org/2002/07/owl#> .\n");

    for class in classes {
        let _ = write!(
            out,
            "\n<{}> a owl:Class ;\n  rdfs:label \"{}\"",
            class.iri(),
            escape(class.name)
        );
        if let Some(text) = description(class.definition) {
            let _ = write!(out, " ;\n  rdfs:comment \"{}\"", escape(text));
        }
        out.push_str(" .\n");

        for (slot, definition) in class.slots() {
            // A slot with a datatype is a DatatypeProperty; one whose shape the projection
            // cannot state stays a plain property rather than being guessed into an
            // ObjectProperty it may not be.
            let kind = match xsd(definition) {
                Some(_) => "owl:DatatypeProperty",
                None => "rdf:Property",
            };
            let _ = write!(
                out,
                "<{}> a {kind} ;\n  rdfs:domain <{}> ;\n  rdfs:label \"{}\"",
                class.slot_iri(slot),
                class.iri(),
                escape(slot)
            );
            if let Some(datatype) = xsd(definition) {
                let _ = write!(out, " ;\n  rdfs:range {datatype}");
            }
            if let Some(text) = description(definition) {
                let _ = write!(out, " ;\n  rdfs:comment \"{}\"", escape(text));
            }
            out.push_str(" .\n");
        }
    }
    out
}

/// The RDFS rendering of the projected model (EP-46).
pub fn rdf(classes: &[Class]) -> String {
    let mut out = turtle_header("RDF Schema rendering");

    for class in classes {
        let _ = write!(
            out,
            "\n<{}> a rdfs:Class ;\n  rdfs:label \"{}\" .\n",
            class.iri(),
            escape(class.name)
        );
        for (slot, definition) in class.slots() {
            let _ = write!(
                out,
                "<{}> a rdf:Property ;\n  rdfs:domain <{}> ;\n  rdfs:label \"{}\"",
                class.slot_iri(slot),
                class.iri(),
                escape(slot)
            );
            if let Some(datatype) = xsd(definition) {
                let _ = write!(out, " ;\n  rdfs:range {datatype}");
            }
            out.push_str(" .\n");
        }
    }
    out
}

/// The LinkML range of a projected slot; an absent one is LinkML's `string` default.
fn linkml_range(definition: &Value) -> Option<&'static str> {
    match xsd(definition)? {
        "xsd:anyURI" => Some("uriorcurie"),
        "xsd:dateTime" => Some("datetime"),
        "xsd:date" => Some("date"),
        "xsd:time" => Some("time"),
        "xsd:double" => Some("float"),
        "xsd:integer" => Some("integer"),
        "xsd:boolean" => Some("boolean"),
        _ => Some("string"),
    }
}

/// The LinkML source of the projected model, one YAML document per model (EP-46, DM-01).
///
/// Several models can share one major, and a LinkML schema has one `id`, so they are
/// separate documents in one stream rather than one merged schema with an invented identity.
pub fn linkml(models: &[&Model], classes: &[Class]) -> String {
    let mut documents = Vec::new();

    for model in models {
        let mine: Vec<&Class> = classes
            .iter()
            .filter(|class| class.model.name == model.name && class.model.major == model.major)
            .collect();
        if mine.is_empty() {
            continue;
        }

        let mut out = String::new();
        let _ = write!(
            out,
            "# LinkML source, rendered by the joinedcontext gateway from the model this\n\
             # endpoint grants. Classes and slots the caller may not read are absent (EP-47).\n\
             id: urn:joinedcontext:model:{}:v{}\n\
             name: {}\n\
             version: \"{}\"\n\
             prefixes:\n  linkml: https://w3id.org/linkml/\n\
             default_range: string\n\
             imports:\n  - linkml:types\n\
             classes:\n",
            model.name, model.major, model.name, model.version
        );
        for class in mine {
            let _ = writeln!(out, "  {}:", class.name);
            if let Some(text) = description(class.definition) {
                let _ = writeln!(out, "    description: {}", yaml_scalar(text));
            }
            let slots = class.slots();
            if slots.is_empty() {
                out.push_str("    attributes: {}\n");
                continue;
            }
            out.push_str("    attributes:\n");
            for (slot, definition) in slots {
                let _ = writeln!(out, "      {slot}:");
                if let Some(range) = linkml_range(definition) {
                    let _ = writeln!(out, "        range: {range}");
                }
                if multivalued(definition) {
                    out.push_str("        multivalued: true\n");
                }
                if class.requires(slot) {
                    out.push_str("        required: true\n");
                }
                if let Some(values) = enumeration(definition) {
                    let members: Vec<String> = values
                        .iter()
                        .map(|value| match value {
                            Value::String(text) => yaml_scalar(text),
                            other => other.to_string(),
                        })
                        .collect();
                    let _ = writeln!(out, "        permissible_values: [{}]", members.join(", "));
                }
                if let Some(text) = description(definition) {
                    let _ = writeln!(out, "        description: {}", yaml_scalar(text));
                }
            }
        }
        documents.push(out);
    }

    documents.join("---\n")
}

/// A YAML scalar that survives a colon, a quote or a leading indicator character.
fn yaml_scalar(text: &str) -> String {
    format!("\"{}\"", escape(text))
}

/// The human documentation of the projected model (EP-46, DM-02).
pub fn markdown(models: &[&Model], classes: &[Class]) -> String {
    let title: Vec<String> = models
        .iter()
        .map(|model| format!("{} {}", model.name, model.version))
        .collect();
    let mut out = format!("# {}\n\n", title.join(", "));
    out.push_str(
        "What this endpoint publishes about its data, narrowed to your grant: a type or an \
         attribute you may not read is absent from this page and from every other formalism \
         on the schema surface.\n",
    );

    for class in classes {
        let _ = writeln!(out, "\n## {}", class.name);
        if let Some(text) = description(class.definition) {
            let _ = writeln!(out, "\n{text}");
        }
        let slots = class.slots();
        if slots.is_empty() {
            out.push_str("\nNo readable attributes.\n");
            continue;
        }
        out.push_str("\n| Attribute | Type | Required | Description |\n|---|---|---|---|\n");
        for (slot, definition) in slots {
            let range = linkml_range(definition).unwrap_or("any");
            let list = if multivalued(definition) {
                " (list)"
            } else {
                ""
            };
            let required = if class.requires(slot) { "yes" } else { "no" };
            let text = description(definition).unwrap_or("").replace('|', "\\|");
            let _ = writeln!(out, "| {slot} | {range}{list} | {required} | {text} |");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn model(name: &str, major: u32) -> Model {
        Model {
            name: name.to_owned(),
            version: format!("{major}.0.0"),
            major,
            classes: Vec::new(),
            json_schema: None,
            context: None,
        }
    }

    /// One class as the projection leaves it: the JSON Schema definition, already narrowed.
    fn class<'a>(name: &'a str, model: &'a Model, definition: &'a Value) -> Class<'a> {
        Class {
            name,
            model,
            definition,
        }
    }

    /// T-1158, EP-46: an endpoint over two models renders both, each under its own namespace,
    /// so a partner can tell which model a shape belongs to.
    #[test]
    fn two_models_render_as_two_shapes_under_two_namespaces() {
        let air = model("air-quality", 1);
        let transport = model("transport", 2);
        let observed = json!({ "properties": { "pm10": { "type": "number" } } });
        let vehicle = json!({ "properties": { "speed": { "type": "number" } } });

        let shapes = shacl(&[
            class("AirQualityObserved", &air, &observed),
            class("Vehicle", &transport, &vehicle),
        ]);

        assert!(shapes.contains("urn:joinedcontext:model:air-quality:v1:AirQualityObservedShape"));
        assert!(shapes.contains("urn:joinedcontext:model:transport:v2:VehicleShape"));
        assert_eq!(shapes.matches("a sh:NodeShape").count(), 2, "{shapes}");
    }

    #[test]
    fn a_class_with_no_slots_is_a_shape_with_no_properties() {
        let air = model("air-quality", 1);
        let empty = json!({ "properties": {} });
        let shapes = shacl(&[class("AirQualityObserved", &air, &empty)]);

        assert!(shapes.contains("a sh:NodeShape"));
        assert!(!shapes.contains("sh:property"), "{shapes}");

        let ontology = owl(&[class("AirQualityObserved", &air, &empty)]);
        assert!(ontology.contains("a owl:Class"));
        assert!(!ontology.contains("owl:DatatypeProperty"), "{ontology}");
    }

    /// Nothing to render is a document with its header and nothing else: a partner reading it
    /// learns the endpoint publishes no class, which is different from a broken document.
    #[test]
    fn no_class_at_all_still_renders_a_document() {
        for text in [shacl(&[]), owl(&[]), rdf(&[])] {
            assert!(text.contains("@prefix"), "{text}");
            assert!(!text.contains("NodeShape"), "{text}");
            assert!(!text.contains("owl:Class"), "{text}");
        }
    }

    #[test]
    fn an_enumeration_is_a_shacl_in_list_and_a_multivalued_slot_has_no_ceiling() {
        let air = model("air-quality", 1);
        let definition = json!({
            "properties": {
                "quality": { "type": "string", "enum": ["good", "poor"] },
                "readings": { "type": "array", "items": { "type": "number" } }
            },
            "required": ["quality"]
        });
        let shapes = shacl(&[class("AirQualityObserved", &air, &definition)]);

        assert!(shapes.contains(r#"sh:in ( "good" "poor" )"#), "{shapes}");
        assert!(
            shapes.contains("sh:minCount 1"),
            "a required slot: {shapes}"
        );
        // The multivalued one takes no `sh:maxCount`, which is what makes it many.
        let readings = shapes
            .split("sh:property")
            .find(|part| part.contains("\"readings\""))
            .expect("the readings property");
        assert!(!readings.contains("sh:maxCount"), "{readings}");
    }

    /// The LinkML the surface serves is one document per model, in the order they were given.
    #[test]
    fn linkml_carries_one_document_per_model() {
        let air = model("air-quality", 1);
        let transport = model("transport", 2);
        let observed = json!({ "properties": { "pm10": { "type": "number" } } });
        let vehicle = json!({ "properties": { "speed": { "type": "number" } } });

        let source = linkml(
            &[&air, &transport],
            &[
                class("AirQualityObserved", &air, &observed),
                class("Vehicle", &transport, &vehicle),
            ],
        );

        assert!(source.contains("air-quality"), "{source}");
        assert!(source.contains("transport"), "{source}");
        assert!(source.contains("AirQualityObserved"), "{source}");
        assert!(source.contains("Vehicle"), "{source}");
    }

    /// A description with a pipe in it must not break the table it is written into.
    #[test]
    fn a_description_never_breaks_the_markdown_table_it_is_written_into() {
        let air = model("air-quality", 1);
        let definition = json!({
            "properties": {
                "pm10": {
                    "type": "number",
                    "description": "particulate matter | under 10 µm"
                }
            }
        });
        let doc = markdown(&[&air], &[class("AirQualityObserved", &air, &definition)]);

        let row = doc
            .lines()
            .find(|line| line.contains("pm10"))
            .expect("the slot's row");
        // Four columns: slot, range, required, description. A raw pipe would make five.
        assert_eq!(
            row.matches('|').count() - row.matches("\\|").count(),
            5,
            "a pipe in the description opened a column: {row}"
        );
    }
}
