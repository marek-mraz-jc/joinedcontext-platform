//! Every field of every kind carries documentation, and none of it carries a secret
//! (T-2145, UI-02).
//!
//! The rustdoc of a `jc-core` field is not only a comment. `registry::schema_of` turns it
//! into the JSON Schema `description` that the Portal ships in `ui/src/schemas/kinds`, the
//! API publishes through utoipa and the MCP tool list hands a model. A field without one is
//! a field that is unexplained in four places at once, and a doc comment holding a
//! credential-shaped value is that value published in all four.
//!
//! What a person filling a form reads is a different string: the UiSchema's `help`, written
//! per field and per locale, which `ui/tests/form_help.test.ts` holds to one sentence in each
//! of the four shipped locales. The `description` is the fallback behind it, for whoever is
//! reading the API rather than the form (`ui/src/components/forms/theme.tsx`, T-1604). So
//! these cases ask that it exists and is safe, not that it is prose.

use jc_core::registry::{self, KINDS};
use serde_json::Value;

/// The one property whose description lives on its parent rather than on itself.
///
/// `ProjectCreation::Group(String)` is a newtype variant: `schemars` renders it as an object
/// with a single `Group` member, and the variant's own doc comment — "The members of one
/// group." — goes on that object. Documenting the inner member would repeat the sentence a
/// reader has just read one line above, so the parent is asserted instead.
const DOCUMENTED_BY_ITS_PARENT: &[(&str, &str)] = &[("Organization", "Group")];

/// Values that must never appear in a doc comment, because a doc comment is published.
///
/// The shapes are the ones a credential announces itself with. A placeholder that names its
/// own shape (`sha256:...`, `ghp_<token>`) is prose about a format and is not one of these:
/// each pattern needs the length a real credential has.
/// One credential shape: what it is called, and how it is recognised.
struct Shape {
    name: &'static str,
    matches: fn(&str) -> bool,
}

fn looks_like_a_credential(text: &str) -> Option<&'static str> {
    const fn shape(name: &'static str, matches: fn(&str) -> bool) -> Shape {
        Shape { name, matches }
    }
    let shapes = [
        shape("a GitHub token", |t| has_prefixed_secret(t, "ghp_", 36)),
        shape("a GitHub app token", |t| {
            has_prefixed_secret(t, "github_pat_", 22)
        }),
        shape("an OpenAI key", |t| has_prefixed_secret(t, "sk-", 20)),
        shape("an AWS access key id", |t| {
            has_prefixed_secret(t, "AKIA", 16)
        }),
        shape("a Slack token", |t| has_prefixed_secret(t, "xoxb-", 10)),
        shape("a JSON Web Token", |t| {
            t.split(|c: char| !(c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-'))
                .any(|word| {
                    let parts: Vec<&str> = word.split('.').collect();
                    parts.len() == 3
                        && word.starts_with("eyJ")
                        && parts.iter().all(|part| part.len() >= 8)
                })
        }),
        shape("a PEM private key", |t| t.contains("PRIVATE KEY-----")),
    ];
    shapes
        .iter()
        .find(|shape| (shape.matches)(text))
        .map(|shape| shape.name)
}

/// Whether the text holds `prefix` followed by at least `length` credential characters.
fn has_prefixed_secret(text: &str, prefix: &str, length: usize) -> bool {
    text.match_indices(prefix).any(|(at, _)| {
        text[at + prefix.len()..]
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
            .count()
            >= length
    })
}

/// One property of one kind's schema, wherever in the tree it sits.
struct Property {
    kind: &'static str,
    /// The dotted path of the object that holds it, for a message a person can follow.
    path: String,
    name: String,
    schema: Value,
}

/// Every property of every kind's schema.
fn properties() -> Vec<Property> {
    let mut found = Vec::new();
    for info in KINDS {
        let schema = registry::schema_of(info.kind)
            .unwrap_or_else(|| panic!("no schema for kind {}", info.kind));
        collect(info.kind, String::new(), &schema, &mut found);
    }
    found
}

fn collect(kind: &'static str, path: String, node: &Value, found: &mut Vec<Property>) {
    match node {
        Value::Object(members) => {
            if let Some(Value::Object(props)) = members.get("properties") {
                for (name, sub) in props {
                    found.push(Property {
                        kind,
                        path: path.clone(),
                        name: name.clone(),
                        schema: sub.clone(),
                    });
                }
            }
            for (key, value) in members {
                collect(kind, format!("{path}.{key}"), value, found);
            }
        }
        Value::Array(items) => {
            for (index, value) in items.iter().enumerate() {
                collect(kind, format!("{path}[{index}]"), value, found);
            }
        }
        _ => {}
    }
}

/// UI-02: a field with no `description` is a field the API, the form fallback and the MCP
/// tool list all present without a word of explanation. The one exception is named above and
/// is asserted at its parent, so the exemption cannot quietly grow.
#[test]
fn every_kind_field_carries_a_description() {
    let all = properties();
    assert!(
        all.len() > 1_000,
        "{} properties is too few to be the whole catalogue",
        all.len(),
    );

    let mut undocumented = Vec::new();
    for property in &all {
        // A `$ref` or an `allOf` carries the referenced schema's own description.
        if property.schema.get("$ref").is_some() || property.schema.get("allOf").is_some() {
            continue;
        }
        let described = property
            .schema
            .get("description")
            .and_then(Value::as_str)
            .is_some_and(|text| !text.trim().is_empty());
        if !described
            && !DOCUMENTED_BY_ITS_PARENT.contains(&(property.kind, property.name.as_str()))
        {
            undocumented.push(format!(
                "{}{}.{}",
                property.kind, property.path, property.name
            ));
        }
    }
    assert!(
        undocumented.is_empty(),
        "{} field(s) of the manifest contract carry no description:\n  {}",
        undocumented.len(),
        undocumented.join("\n  "),
    );
}

/// The exemption above is only an exemption because the sentence is one line up. If the
/// parent ever loses its description, the member is undocumented after all and this fails
/// rather than the case above quietly passing.
#[test]
fn the_field_documented_by_its_parent_really_has_a_documented_parent() {
    for (kind, name) in DOCUMENTED_BY_ITS_PARENT {
        let schema = registry::schema_of(kind).unwrap_or_else(|| panic!("no schema for {kind}"));
        let parents = properties()
            .into_iter()
            .filter(|property| property.kind == *kind && property.name == *name)
            .count();
        assert!(parents > 0, "{kind} has no member {name} any more");

        let text = serde_json::to_string(&schema).expect("the schema serializes");
        assert!(
            text.contains("The members of one group."),
            "{kind}: the parent of {name} no longer carries the sentence that documents it",
        );
    }
}

/// T-2145: a doc comment is published — into the schema, the OpenAPI document and every MCP
/// tool list. A credential-shaped value in one is that value published in all of them.
#[test]
fn no_kind_description_holds_a_credential() {
    let mut leaked = Vec::new();
    for info in KINDS {
        let schema = registry::schema_of(info.kind)
            .unwrap_or_else(|| panic!("no schema for kind {}", info.kind));
        walk_descriptions(&schema, &mut |text| {
            if let Some(shape) = looks_like_a_credential(text) {
                leaked.push(format!("{}: {shape} in {text:.80}", info.kind));
            }
        });
    }
    assert!(
        leaked.is_empty(),
        "a doc comment carries a credential:\n  {}",
        leaked.join("\n  "),
    );
}

/// The credential probe itself: it catches what it is for, and leaves a placeholder alone.
/// Without this, a probe that matched nothing at all would make the case above pass for ever.
#[test]
fn the_credential_probe_knows_a_credential_from_a_placeholder() {
    for (text, expected) in [
        (
            "the token, ghp_0123456789abcdefghijklmnopqrstuvwxyz",
            Some("a GitHub token"),
        ),
        (
            "eyJhbGciOiJFUzI1NiJ9.eyJzdWIiOiJqYW5hIn0.c2lnbmF0dXJlLWhlcmU",
            Some("a JSON Web Token"),
        ),
        ("-----BEGIN RSA PRIVATE KEY-----", Some("a PEM private key")),
        ("AKIAIOSFODNN7EXAMPLE", Some("an AWS access key id")),
        // Placeholders and prose about a format, which every kind is full of.
        (
            "The artifact's digest, `sha256:` and 64 lowercase hexadecimal characters.",
            None,
        ),
        (
            "A forge token, named by a secretRef; never a literal (`ghp_…`).",
            None,
        ),
        ("Bearer eyJhbGciOi…", None),
        (
            "The key is `sk-` followed by the provider's own characters.",
            None,
        ),
        ("", None),
    ] {
        assert_eq!(
            looks_like_a_credential(text),
            expected,
            "the probe read {text:?} wrongly",
        );
    }
}

fn walk_descriptions(node: &Value, seen: &mut impl FnMut(&str)) {
    match node {
        Value::Object(members) => {
            if let Some(Value::String(text)) = members.get("description") {
                seen(text);
            }
            for value in members.values() {
                walk_descriptions(value, seen);
            }
        }
        Value::Array(items) => {
            for value in items {
                walk_descriptions(value, seen);
            }
        }
        _ => {}
    }
}
