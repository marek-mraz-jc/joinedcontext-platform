//! Cutting the response down to what the grants cover (T-0151, R9, GW11).
//!
//! The broker answers with whole entities. What the caller is allowed to see is usually
//! less than that, and the difference must never reach the wire: an attribute omitted
//! from a grant is not a display preference, it is data the caller has no right to.
//!
//! Projection preserves NGSI-LD shape. `id`, `type` and the JSON-LD keywords stay, because
//! an entity without them is not an entity; every other member survives only if the
//! constraint set names it.

use super::evaluator::Constraints;
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

/// Strips a whole broker answer, whether it is one entity or an array of them (R9, EP-61).
pub fn project(body: &mut Value, granted: &BTreeSet<String>, hidden: &BTreeSet<String>) {
    match body {
        Value::Array(entities) => {
            for entity in entities {
                project_entity(entity, granted, hidden);
            }
        }
        entity => project_entity(entity, granted, hidden),
    }
}

/// The same, by the constraint set, so each entity is stripped by the slots of its own type
/// (MP-02, T-1862).
///
/// `constraints.attrs` is the union the broker was given, which is a superset for every entity
/// whose type owns only part of it. Where a projection says which slots each class has, the
/// answer is cut by the entity's own types instead: a `Vehicle` in an answer to
/// `type=User,Vehicle` keeps `weight` and never the `age` the projection gives to a `User`.
pub fn project_by_type(body: &mut Value, constraints: &Constraints) {
    // The same areas the answer itself is filtered by, so an entity the broker inlined under
    // a Relationship is judged by every rule a top-level one is (T-1859).
    let areas = super::geo::Areas::of(&constraints.geo_grants, constraints.geo_caller.as_deref());
    match body {
        Value::Array(entities) => {
            for entity in entities {
                entity_by_type(entity, constraints, areas.as_ref());
            }
        }
        entity => entity_by_type(entity, constraints, areas.as_ref()),
    }
}

/// One entity, stripped by the attributes its own types may serve, and then by what a join
/// inlined into it.
fn entity_by_type(
    entity: &mut Value,
    constraints: &Constraints,
    areas: Option<&super::geo::Areas>,
) {
    retain_by_type(entity, constraints);
    narrow_joined(entity, constraints, areas, JOIN_DEPTH);
}

/// The deepest chain of inlined entities the gateway walks, which is the bound the read
/// surface clamps `joinLevel` to (CIM 009 clause 6.3.11, [`crate::query`]).
const JOIN_DEPTH: usize = 3;

/// An entity the broker inlined under a Relationship (`join=inline`) is an entity the caller
/// reads, so it is judged and cut exactly like a top-level one (EP-26, R9, T-1859).
///
/// The projection above retains whole members, and an inlined entity travels inside one: a
/// granted Relationship would otherwise carry a whole entity of a type, a space or an area no
/// grant reaches. What stays either way is the Relationship's `object`, the URN — the link is
/// not the secret, the entity behind it is.
fn narrow_joined(
    entity: &mut Value,
    constraints: &Constraints,
    areas: Option<&super::geo::Areas>,
    depth: usize,
) {
    let Some(members) = entity.as_object_mut() else {
        return;
    };
    for value in members.values_mut() {
        // An attribute is one instance or, with `datasetId`, a list of them.
        let instances: Vec<&mut Value> = match value {
            Value::Array(list) => list.iter_mut().collect(),
            other => vec![other],
        };
        for instance in instances {
            let Some(attribute) = instance.as_object_mut() else {
                continue;
            };
            let Some(joined) = attribute.get_mut("entity") else {
                continue;
            };
            if depth == 0
                || !permitted(joined, constraints)
                || !areas.is_none_or(|areas| areas.admits(joined))
            {
                attribute.remove("entity");
                continue;
            }
            retain_by_type(joined, constraints);
            narrow_joined(joined, constraints, areas, depth - 1);
        }
    }
}

/// One entity, stripped by the attributes its own types may serve.
fn retain_by_type(entity: &mut Value, constraints: &Constraints) {
    if constraints.attrs_by_type.is_empty() {
        project_entity(entity, &constraints.attrs, &constraints.hidden);
        return;
    }
    // A type the map does not name is a type this endpoint serves nothing of: identity only,
    // never the union. An entity with several types keeps the union over the ones it is granted,
    // because each of them is a type the caller may read it as.
    let mut allowed: BTreeSet<String> = BTreeSet::new();
    for name in types_of(entity) {
        if let Some(slots) = constraints.attrs_by_type.get(&name) {
            allowed.extend(slots.iter().cloned());
        }
    }
    project_entity_to(entity, &allowed, &constraints.hidden);
}

/// The type or types one entity declares, as plain names.
fn types_of(entity: &Value) -> Vec<String> {
    match entity.get("type").or_else(|| entity.get("@type")) {
        Some(Value::String(one)) => vec![one.clone()],
        Some(Value::Array(many)) => many
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
        _ => Vec::new(),
    }
}

/// [`project_entity`] with no "empty means everything" rule: an empty `allowed` set is an
/// entity cut to its identity, which is what a type outside the projection gets.
fn project_entity_to(entity: &mut Value, allowed: &BTreeSet<String>, hidden: &BTreeSet<String>) {
    let Some(members) = entity.as_object_mut() else {
        return;
    };
    members.retain(|name, _| {
        STRUCTURAL.contains(&name.as_str())
            || SYSTEM.contains(&name.as_str())
            || (allowed.contains(name) && !hidden.contains(name))
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

/// Whether an entity may be returned at all, given the types and the anchored id patterns of
/// the matching grants (EP-26, R24, GW11).
///
/// The one door every read goes through, which is why both halves live here: a query is
/// narrowed upstream with `?type=`, but a retrieve by id carries no type at all, and a broker
/// is free to answer an entity of any type it likes. The broker is not the authority on what a
/// caller may read, so the type of what came back is judged here (T-2130).
pub fn permitted(entity: &Value, constraints: &Constraints) -> bool {
    // Only an object is an entity. An array, a string or a number carries no type and no id to
    // judge, so under a grant that narrows neither — an ordinary attribute grant — both halves
    // below would say yes to it, and `project_entity_to` leaves anything but an object alone:
    // a nested array would reach the caller with every attribute it carries (T-2335, T-2131).
    if !entity.is_object() {
        return false;
    }
    type_granted(entity, constraints) && id_permitted(entity, &constraints.id_patterns)
}

/// Whether the type the answer declares is one the grants name (EP-26).
///
/// An empty set is a grant over every type the endpoint serves. An entity carrying several
/// types is granted when any one of them is, which is how NGSI-LD multi-typing works: the
/// grant is a statement about a type, not about a type being the only one. An answer with no
/// type at all cannot be judged, so under a type grant it is not served. Types compare as IRIs
/// ([`type_named`]), so another vocabulary's `Depot` is not the grant's (T-3473).
fn type_granted(entity: &Value, constraints: &Constraints) -> bool {
    if constraints.types.is_empty() {
        return true;
    }
    let declared = entity.get("type").or_else(|| entity.get("@type"));
    match declared {
        Some(Value::String(one)) => type_named(constraints, one),
        Some(Value::Array(several)) => several
            .iter()
            .filter_map(Value::as_str)
            .any(|one| type_named(constraints, one)),
        _ => false,
    }
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

/// Whether `name`, compacted under the core context or expanded, is one of the granted types,
/// compared as IRIs: the core context's reading of each granted name, and what the space's own
/// `@context`s make of it (`type_iris`). A name the gateway cannot expand grants nothing.
pub fn type_named(constraints: &Constraints, name: &str) -> bool {
    type_iri(name).is_some_and(|asked| {
        constraints.type_iris.contains(&asked)
            || constraints
                .types
                .iter()
                .any(|one| type_iri(one).is_some_and(|grant| grant == asked))
    })
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

/// The last segment of an IRI, after `#` or `/`; a plain term is returned unchanged.
///
/// It picks attribute slots for a name, never an access decision: two vocabularies' `Depot`
/// share it, so a type is granted by [`type_named`], which compares IRIs (T-3473).
pub fn term(iri: &str) -> &str {
    iri.rsplit(['#', '/']).next().unwrap_or(iri)
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
