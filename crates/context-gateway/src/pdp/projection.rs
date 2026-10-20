//! Cutting the response down to what the grants cover (T-0151, R9, GW11).
//!
//! The broker answers with whole entities. What the caller is allowed to see is usually
//! less than that, and the difference must never reach the wire: an attribute omitted
//! from a grant is not a display preference, it is data the caller has no right to.
//!
//! Projection preserves NGSI-LD shape. The system members of DM-130 (`id`, `type`, `scope`,
//! `@context`, the broker's timestamps and `expiresAt`) stay, because an entity without them is
//! not an entity and a grant is about data, not about when it was written or how long it lives
//! (EP-71, DM-131); every other member survives only if the constraint set names it.

use jc_core::system_members::is_entity_system_member;
use serde_json::Value;
use std::collections::BTreeSet;

/// The system members only the broker sets (CIM 009 clause 4.8).
const BROKER_SET: [&str; 3] = ["createdAt", "modifiedAt", "deletedAt"];

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
        is_entity_system_member(name)
            || ((granted.is_empty() || granted.contains(name)) && !hidden.contains(name))
    });
}

/// The attributes an entity carries that the grants do not cover (R9, GW17).
///
/// A read strips them; a write that touches any of them is denied whole, because silently
/// dropping an attribute from a write would store something the caller did not ask for. A system
/// member is no attribute, so no grant needs to name it: who may write the entity may set its
/// `expiresAt` (DM-131, PL-84). The timestamps the broker sets are not a client's to send, so a
/// write carrying one is still outside a whitelist (GW17).
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
        .filter(|name| {
            (!is_entity_system_member(name) || BROKER_SET.contains(name))
                && !granted.contains(*name)
        })
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
