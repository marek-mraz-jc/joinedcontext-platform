//! Cutting the response down to what the grants cover (T-0151, R9, GW11).
//!
//! The broker answers with whole entities. What the caller is allowed to see is usually
//! less than that, and the difference must never reach the wire: an attribute omitted
//! from a grant is not a display preference, it is data the caller has no right to.
//!
//! Projection preserves NGSI-LD shape. `id`, `type` and the JSON-LD keywords stay, because
//! an entity without them is not an entity; every other member survives only if the
//! constraint set names it.

use serde_json::Value;
use std::collections::BTreeSet;

/// The members every entity keeps, whatever the grants say: without them the answer is
/// not a valid NGSI-LD entity.
const STRUCTURAL: &[&str] = &["id", "type", "@context", "@id", "@type", "scope"];

/// The members the broker generates rather than the author writing them, which a read keeps
/// whatever the grants name (EP-71, CIM 009 clause 4.8).
///
/// A grant is a statement about the data a caller may see, not about whether they may know
/// when it was written or which registered source answered. On a federated endpoint that
/// distinction is the whole of provenance: strip these and a merged answer stops saying where
/// any of it came from, which is what EP-71 exists to prevent.
///
/// A read only. [`ungranted`] deliberately does not know about this list, so a write that
/// carries one of these members is still refused: they are the broker's to set, never a
/// client's to send.
const SYSTEM: &[&str] = &["createdAt", "modifiedAt", "deletedAt", "expiresAt"];

/// Strips from one entity every attribute the grants do not name and every attribute the
/// endpoint hides (R9, EP-61).
///
/// An empty `granted` set means the grants named no attribute whitelist at all, which is
/// a grant over the whole entity. `hidden` is the opposite kind of set: a denial that
/// applies whether or not there is a whitelist, which is why the endpoint's narrowing
/// cannot be expressed by shrinking `granted`.
pub fn project_entity(entity: &mut Value, granted: &BTreeSet<String>, hidden: &BTreeSet<String>) {
    if granted.is_empty() && hidden.is_empty() {
        return;
    }
    let Some(members) = entity.as_object_mut() else {
        return;
    };
    members.retain(|name, _| {
        STRUCTURAL.contains(&name.as_str())
            || SYSTEM.contains(&name.as_str())
            || ((granted.is_empty() || granted.contains(name)) && !hidden.contains(name))
    });
}

/// The attributes an entity carries that the grants do not cover (R9, GW17).
///
/// A read strips them; a write that touches any of them is denied whole, because silently
/// dropping an attribute from a write would store something the caller did not ask for.
pub fn ungranted<'a>(entity: &'a Value, granted: &BTreeSet<String>) -> Vec<&'a str> {
    if granted.is_empty() {
        return Vec::new();
    }
    let Some(members) = entity.as_object() else {
        return Vec::new();
    };
    members
        .keys()
        .map(String::as_str)
        .filter(|name| !STRUCTURAL.contains(name) && !granted.contains(*name))
        .collect()
}

/// The NGSI-LD default vocabulary: where the core context puts a term it does not define.
const DEFAULT_VOCAB: &str = "https://uri.etsi.org/ngsi-ld/default-context/";

/// A type name as the IRI it names under the NGSI-LD core context, the only context the
/// gateway admits (T-3287): a term is in the default vocabulary, an absolute IRI is itself.
/// `None` for a name the gateway cannot expand: a compact IRI (`other:Depot`) names a prefix
/// the core context does not define, and an empty name names nothing.
pub fn type_iri(name: &str) -> Option<String> {
    if name.contains("://") || name.starts_with("urn:") {
        return Some(name.to_owned());
    }
    if name.is_empty() || name.contains(':') || name.contains(char::is_whitespace) {
        return None;
    }
    Some(format!("{DEFAULT_VOCAB}{name}"))
}

/// What the type names `types` stand for under the `@context` of each model: the term's own
/// definition (an absolute IRI, or `{"@id": …}`), else the model's `@vocab` followed by the
/// term. A definition that is not an absolute IRI adds nothing; the core context's reading
/// is [`type_iri`]'s.
pub fn model_type_iris<'a>(
    types: &BTreeSet<String>,
    contexts: impl Iterator<Item = &'a Value>,
) -> BTreeSet<String> {
    let absolute = |iri: &str| iri.contains("://") || iri.starts_with("urn:");
    let mut out = BTreeSet::new();
    for context in contexts {
        let Some(terms) = crate::handlers::schema::inner_context(context) else {
            continue;
        };
        let vocab = terms
            .get("@vocab")
            .and_then(Value::as_str)
            .filter(|vocab| absolute(vocab));
        for name in types {
            let defined = terms.get(name).and_then(|definition| match definition {
                Value::String(iri) => Some(iri.as_str()),
                Value::Object(spec) => spec.get("@id").and_then(Value::as_str),
                _ => None,
            });
            match (defined, vocab) {
                (Some(iri), _) if absolute(iri) => {
                    out.insert(iri.to_owned());
                }
                (None, Some(vocab)) if type_iri(name).is_some() && !absolute(name) => {
                    out.insert(format!("{vocab}{name}"));
                }
                _ => {}
            }
        }
    }
    out
}

/// Whether the entity's id falls inside the anchored id patterns of the matching grants (R24).
///
/// No pattern is no restriction. A pattern that does not compile matches nothing: a
/// grant the gateway cannot evaluate must not become a grant that lets everything
/// through. `jcctl validate` and the Portal reject an uncompilable pattern before it is
/// committed, so this is the second line, not the first.
pub fn id_permitted(entity: &Value, id_patterns: &BTreeSet<String>) -> bool {
    if id_patterns.is_empty() {
        return true;
    }
    let Some(id) = entity
        .get("id")
        .or_else(|| entity.get("@id"))
        .and_then(Value::as_str)
    else {
        return false;
    };
    id_patterns
        .iter()
        .any(|pattern| regex::Regex::new(pattern).is_ok_and(|compiled| compiled.is_match(id)))
}
