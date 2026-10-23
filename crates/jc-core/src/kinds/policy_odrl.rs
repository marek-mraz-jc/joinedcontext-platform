//! A Policy manifest as an ODRL 2.2 `Set` in the `ngsi-ld:` profile, and such a `Set` back into a
//! Policy manifest (ADR-N-057 decision 4, R52, T-5039). `jcctl policy export` and `import` and the
//! Portal's Policy form read this one mapping; the YAML file stays the only source.
//!
//! The profile is the one the broker's access document speaks (CIM 009 operations as actions,
//! Entity types as targets, `q`, `scopeQ`, `geoQ`, `temporalQ` as left operands), closed: its
//! context and SHACL shapes are `schemas/odrl/ngsi-ld-v1/`. [`import`] refuses, naming the member,
//! everything the profile does not map (a duty, an unknown left operand or action, a conflict
//! other than `prohibit`, an `Offer` or `Agreement`, any member it does not know), so an imported
//! document can never grant more than it says.

use crate::envelope::{ObjectMeta, Ref, TypedRef};
use crate::kinds::policy::{
    EntitySelector, OperationRef, PlatformAction, PolicyEffect, PolicyRule, PolicySpec, Principal,
    PrincipalKind, RegistrationInfo, Validity,
};
use crate::kinds::Policy;
use crate::urn::Urn;
use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::{json, Map, Value};

/// The JSON-LD contexts of the profile, in this order: ODRL's own, then the `ngsi-ld:` profile.
pub const CONTEXTS: [&str; 2] = [
    "http://www.w3.org/ns/odrl.jsonld",
    "https://joinedcontext.com/odrl/ngsi-ld/v1/context.jsonld",
];

/// The `uid` prefix that names the manifest a `Set` was rendered from.
const UID: &str = "urn:joinedcontext:policy:";

/// The members a `Set` may carry.
const SET_MEMBERS: &[&str] = &[
    "@context",
    "@type",
    "uid",
    "assigner",
    "assignee",
    "conflict",
    "permission",
    "prohibition",
];

/// A document the profile does not map, with the member that does not.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{member}: {reason}")]
pub struct Refusal {
    /// Where the document says it, as a path (`permission[0].duty`).
    pub member: String,
    /// What to change.
    pub reason: String,
}

type Out<T> = std::result::Result<T, Refusal>;

fn refuse<T>(member: impl Into<String>, reason: impl Into<String>) -> Out<T> {
    Err(Refusal {
        member: member.into(),
        reason: reason.into(),
    })
}

/// The manifest identity an import writes, where the `Set` does not name it or should not.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Names {
    /// The project (`metadata.namespace`).
    pub project: Option<String>,
    /// The Context Space (`spec.contextSpaceRef`).
    pub space: Option<String>,
    /// The Policy name (`metadata.name`).
    pub name: Option<String>,
}

/// The Policy as an ODRL `Set`: one rule per entity selector, as the broker writes them.
pub fn render(policy: &Policy) -> Out<Value> {
    let name = &policy.metadata.name;
    let Some(project) = policy.metadata.namespace.as_deref() else {
        return refuse("metadata.namespace", "a Policy names its project");
    };
    let rules = policy.spec.expand(name).map_err(|e| Refusal {
        member: "spec.rules".to_owned(),
        reason: e.to_string(),
    })?;
    let (mut permission, mut prohibition) = (Vec::new(), Vec::new());
    for (_, rule) in &rules {
        match rule.effect {
            PolicyEffect::Permission => permission.extend(odrl_rules(rule)),
            PolicyEffect::Prohibition => prohibition.extend(odrl_rules(rule)),
        }
    }
    let space = policy.spec.context_space_ref.name();
    let mut set = Map::new();
    set.insert("@context".into(), json!(CONTEXTS));
    set.insert("@type".into(), json!("Set"));
    set.insert(
        "uid".into(),
        json!(format!("{UID}{project}:{space}:{name}")),
    );
    set.insert("assigner".into(), json!(policy.spec.assigner));
    set.insert("conflict".into(), json!("prohibit"));
    set.insert("permission".into(), Value::Array(permission));
    if !prohibition.is_empty() {
        set.insert("prohibition".into(), Value::Array(prohibition));
    }
    Ok(Value::Object(set))
}

/// One single-rule spec as ODRL rules: one per selector, or one without a target.
fn odrl_rules(rule: &PolicySpec) -> Vec<Value> {
    let mut base = Map::new();
    base.insert("assignee".into(), party(&rule.assignee));
    let actions: Vec<String> = rule
        .operations
        .iter()
        .map(|o| format!("ngsi-ld:{}", o.as_str()))
        .chain(rule.actions.iter().map(|a| format!("ngsi-ld:{}", wire(a))))
        .collect();
    base.insert("action".into(), json!(actions));
    let constraints = constraints(rule);
    if !constraints.is_empty() {
        base.insert("constraint".into(), Value::Array(constraints));
    }
    if rule.information.is_empty() {
        return vec![Value::Object(base)];
    }
    let mut out = Vec::new();
    // several entries name theirs: equal names alone would read back as one entry
    let several = rule.information.len() > 1;
    for (info, k) in rule.information.iter().zip(1..) {
        let part = several.then(|| format!("#information-{k}"));
        for selector in &info.entities {
            let mut one = base.clone();
            one.insert("target".into(), target(selector, info, part.as_deref()));
            out.push(Value::Object(one));
        }
    }
    out
}

/// A serde unit variant's wire name.
fn wire(value: &impl serde::Serialize) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default()
}

const PARTIES: [(PrincipalKind, &str); 5] = [
    (PrincipalKind::User, "ngsi-ld:User"),
    (PrincipalKind::Group, "ngsi-ld:Group"),
    (PrincipalKind::Role, "ngsi-ld:Role"),
    (PrincipalKind::ServiceAccount, "ngsi-ld:ServiceAccount"),
    (PrincipalKind::Did, "ngsi-ld:Did"),
];

fn party(principal: &Principal) -> Value {
    let class = PARTIES
        .iter()
        .find(|(kind, _)| *kind == principal.kind)
        .map_or("ngsi-ld:Role", |(_, class)| class);
    json!({"@type": class, "uid": principal.id})
}

fn constraint(left: &str, operator: &str, right: Value) -> Value {
    json!({"leftOperand": left, "operator": operator, "rightOperand": right})
}

/// The residual filters verbatim and the validity window, in that order.
fn constraints(rule: &PolicySpec) -> Vec<Value> {
    let verbatim = [
        ("ngsi-ld:q", "eq", &rule.q),
        ("ngsi-ld:scopeQ", "isPartOf", &rule.scope_q),
        ("ngsi-ld:geoQ", "eq", &rule.geo_q),
        ("ngsi-ld:temporalQ", "eq", &rule.temporal_q),
    ];
    let mut out: Vec<Value> = verbatim
        .into_iter()
        .filter_map(|(left, op, value)| value.as_ref().map(|v| constraint(left, op, json!(v))))
        .collect();
    let validity = rule.validity.as_ref();
    for (op, at) in [
        ("gteq", validity.and_then(|v| v.from)),
        ("lt", validity.and_then(|v| v.to)),
    ] {
        if let Some(at) = at {
            let at = at.to_rfc3339_opts(SecondsFormat::AutoSi, true);
            out.push(constraint("ngsi-ld:validity", op, json!(at)));
        }
    }
    out
}

/// One selector of an `information` entry as an `EntityType` asset with its refinements, `partOf`
/// the entry when the rule has several.
fn target(selector: &EntitySelector, info: &RegistrationInfo, part: Option<&str>) -> Value {
    let mut refinement = Vec::new();
    if let Some(id) = &selector.id {
        refinement.push(constraint("ngsi-ld:id", "eq", json!(id.to_string())));
    }
    if let Some(pattern) = &selector.id_pattern {
        refinement.push(constraint("ngsi-ld:idPattern", "eq", json!(pattern)));
    }
    let (properties, relationships) = (&info.property_names, &info.relationship_names);
    if !properties.is_empty() && properties == relationships {
        refinement.push(constraint("ngsi-ld:attrs", "isAnyOf", json!(properties)));
    } else {
        for (left, names) in [
            ("ngsi-ld:propertyNames", properties),
            ("ngsi-ld:relationshipNames", relationships),
        ] {
            if !names.is_empty() {
                refinement.push(constraint(left, "isAnyOf", json!(names)));
            }
        }
    }
    let mut asset = Map::new();
    asset.insert("@type".into(), json!("ngsi-ld:EntityType"));
    asset.insert("uid".into(), json!(selector.entity_type));
    if !refinement.is_empty() {
        asset.insert("refinement".into(), Value::Array(refinement));
    }
    if let Some(part) = part {
        asset.insert("partOf".into(), json!(part));
    }
    Value::Object(asset)
}

/// The Policy manifest an ODRL `Set` in the profile says, validated like any manifest.
///
/// `names` overrides what the `Set`'s `uid` names; a `Set` written elsewhere has no such `uid`,
/// so its project, space and name come from `names` alone.
pub fn import(document: &Value, names: &Names) -> Out<Policy> {
    let Value::Object(set) = document else {
        return refuse("(document)", "an ODRL policy is one JSON object");
    };
    let (assigner, fallback) = frame(set)?;
    let (project, space, name) = identity(set.get("uid"), names)?;
    let grants = regroup(rules_of(set, &assigner, fallback.as_ref())?);
    let validity = one_validity(&grants)?;
    let spec = spec_of(grants, assigner, &space, validity);
    let policy = Policy::new(ObjectMeta::new(name, project), spec);
    policy.validate().map_err(|e| Refusal {
        member: "(manifest)".to_owned(),
        reason: e.to_string(),
    })?;
    Ok(policy)
}

/// The `Set`'s own members: its context, type, conflict strategy, assigner and the assignee its
/// rules default to.
fn frame(set: &Map<String, Value>) -> Out<(String, Option<Principal>)> {
    closed(set, SET_MEMBERS, "")?;
    if set.get("@context") != Some(&json!(CONTEXTS)) {
        let reason = format!(
            "must be exactly {CONTEXTS:?}: another context could give a term another meaning"
        );
        return refuse("@context", reason);
    }
    match set.get("@type").and_then(Value::as_str) {
        Some("Set") => {}
        Some(other @ ("Offer" | "Agreement")) => {
            let reason = format!(
                "an {other} is negotiated by a connector, not imported; import the Set it grants"
            );
            return refuse("@type", reason);
        }
        _ => return refuse("@type", "must be \"Set\""),
    }
    if set.get("conflict").is_some_and(|c| c != "prohibit") {
        return refuse(
            "conflict",
            "only \"prohibit\" maps: a prohibition wins over every permission (GW4)",
        );
    }
    let assigner = match set.get("assigner") {
        Some(Value::String(a)) if !a.trim().is_empty() => a.clone(),
        _ => {
            return refuse(
                "assigner",
                "name the granting party, e.g. \"did:web:hel.fi\"",
            )
        }
    };
    let fallback = set
        .get("assignee")
        .map(|p| principal(p, "assignee"))
        .transpose()?;
    Ok((assigner, fallback))
}

/// Every permission, then every prohibition, read one ODRL rule at a time.
fn rules_of(
    set: &Map<String, Value>,
    assigner: &str,
    fallback: Option<&Principal>,
) -> Out<Vec<(Grant, Option<Target>)>> {
    let mut parsed = Vec::new();
    for (key, effect) in [
        ("permission", PolicyEffect::Permission),
        ("prohibition", PolicyEffect::Prohibition),
    ] {
        for (n, rule) in list(set.get(key), key)?.iter().enumerate() {
            let at = format!("{key}[{n}]");
            parsed.push(odrl_rule(rule, &at, effect, assigner, fallback)?);
        }
    }
    Ok(parsed)
}

/// The Policy's one validity window, which every rule repeats; refused when a rule differs.
fn one_validity(grants: &[(Grant, Vec<RegistrationInfo>)]) -> Out<Option<Validity>> {
    let Some((first, _)) = grants.first() else {
        return refuse("permission", "the Set grants and prohibits nothing");
    };
    if grants.iter().any(|(g, _)| g.validity != first.validity) {
        return refuse(
            "constraint",
            "every rule carries the same ngsi-ld:validity: it belongs to the whole Policy",
        );
    }
    Ok(first.validity.clone())
}

/// Refuses the first member of `object` outside `allowed`, with a reason for the known ODRL
/// members the profile leaves out.
fn closed(object: &Map<String, Value>, allowed: &[&str], at: &str) -> Out<()> {
    let Some(member) = object.keys().find(|k| !allowed.contains(&k.as_str())) else {
        return Ok(());
    };
    let reason = match member.as_str() {
        "duty" | "obligation" | "consequence" | "remedy" => {
            "duties are not mapped: the platform enforces none, and dropping one would widen the grant"
        }
        "inheritFrom" | "profile" => "a Set imports on its own, without another policy or profile",
        _ => "is not a member of the ngsi-ld: profile",
    };
    let path = if at.is_empty() {
        member.clone()
    } else {
        format!("{at}.{member}")
    };
    refuse(path, reason)
}

/// Project, space and name: `names` first, then the `uid` the renderer writes.
fn identity(uid: Option<&Value>, names: &Names) -> Out<(String, String, String)> {
    let from_uid: Vec<&str> = uid
        .and_then(Value::as_str)
        .and_then(|u| u.strip_prefix(UID))
        .map(|rest| rest.split(':').collect())
        .unwrap_or_default();
    let part = |given: &Option<String>, n: usize| {
        given
            .clone()
            .or_else(|| (from_uid.len() == 3).then(|| from_uid[n].to_owned()))
    };
    match (
        part(&names.project, 0),
        part(&names.space, 1),
        part(&names.name, 2),
    ) {
        (Some(project), Some(space), Some(name)) => Ok((project, space, name)),
        _ => refuse(
            "uid",
            format!("names no Policy ({UID}<project>:<space>:<name>): give --project, --space and --name"),
        ),
    }
}

/// An ODRL party in the profile: a typed party, or a DID.
fn principal(value: &Value, at: &str) -> Out<Principal> {
    match value {
        Value::String(did) if did.starts_with("did:") => Ok(Principal::new(PrincipalKind::Did, did)),
        Value::Object(object) => {
            closed(object, &["@type", "uid"], at)?;
            let class = object.get("@type").and_then(Value::as_str).unwrap_or("");
            let Some((kind, _)) = PARTIES.iter().find(|(_, c)| *c == class) else {
                return refuse(format!("{at}.@type"), "one of ngsi-ld:User, ngsi-ld:Group, ngsi-ld:Role, ngsi-ld:ServiceAccount, ngsi-ld:Did");
            };
            match object.get("uid").and_then(Value::as_str) {
                Some(id) if !id.trim().is_empty() => Ok(Principal::new(*kind, id)),
                _ => refuse(format!("{at}.uid"), "name the party"),
            }
        }
        _ => refuse(
            at,
            "name the party's kind, e.g. {\"@type\": \"ngsi-ld:Role\", \"uid\": \"public\"}, or give a DID",
        ),
    }
}

/// What one ODRL rule grants, apart from its target: rules with equal grants that follow each
/// other were one Policy rule.
#[derive(Debug, Clone, PartialEq)]
struct Grant {
    effect: PolicyEffect,
    assignee: Principal,
    operations: Vec<OperationRef>,
    actions: Vec<PlatformAction>,
    q: Option<String>,
    scope_q: Option<String>,
    geo_q: Option<String>,
    temporal_q: Option<String>,
    validity: Option<Validity>,
}

/// One selector with its attribute names and the `information` entry it is `partOf`, if named.
type Target = (EntitySelector, Vec<String>, Vec<String>, Option<String>);

fn odrl_rule(
    rule: &Value,
    at: &str,
    effect: PolicyEffect,
    assigner: &str,
    fallback: Option<&Principal>,
) -> Out<(Grant, Option<Target>)> {
    let Value::Object(rule) = rule else {
        return refuse(at, "a rule is a JSON object");
    };
    closed(
        rule,
        &["action", "target", "assignee", "assigner", "constraint"],
        at,
    )?;
    if let Some(own) = rule.get("assigner") {
        if own != assigner {
            return refuse(
                format!("{at}.assigner"),
                "a Policy has one assigner: the Set's",
            );
        }
    }
    let assignee = match (rule.get("assignee"), fallback) {
        (Some(p), _) => principal(p, &format!("{at}.assignee"))?,
        (None, Some(p)) => p.clone(),
        (None, None) => return refuse(format!("{at}.assignee"), "name who the rule is for"),
    };
    let (operations, actions) = actions(rule.get("action"), &format!("{at}.action"))?;
    let mut grant = Grant {
        effect,
        assignee,
        operations,
        actions,
        q: None,
        scope_q: None,
        geo_q: None,
        temporal_q: None,
        validity: None,
    };
    filters(
        &mut grant,
        rule.get("constraint"),
        &format!("{at}.constraint"),
    )?;
    let target = match rule.get("target") {
        Some(t) => Some(asset(t, &format!("{at}.target"))?),
        None => None,
    };
    Ok((grant, target))
}

fn actions(value: Option<&Value>, at: &str) -> Out<(Vec<OperationRef>, Vec<PlatformAction>)> {
    let Some(Value::Array(list)) = value else {
        return refuse(
            at,
            "list the CIM 009 operations, e.g. [\"ngsi-ld:queryEntity\"]",
        );
    };
    let (mut operations, mut platform) = (Vec::new(), Vec::new());
    for (n, item) in list.iter().enumerate() {
        let local = item.as_str().and_then(|s| s.strip_prefix("ngsi-ld:"));
        let Some(local) = local else {
            return refuse(format!("{at}[{n}]"), "only ngsi-ld: actions map: a CIM 009 operation or group, readVersions or personalData");
        };
        if let Ok(op) = serde_json::from_value::<OperationRef>(json!(local)) {
            operations.push(op);
        } else if let Ok(action) = serde_json::from_value::<PlatformAction>(json!(local)) {
            platform.push(action);
        } else {
            return refuse(
                format!("{at}[{n}]"),
                format!("unknown action ngsi-ld:{local}"),
            );
        }
    }
    Ok((operations, platform))
}

/// A constraint's three members, the right operand as given.
fn triple<'a>(item: &'a Value, at: &str) -> Out<(&'a str, &'a str, &'a Value)> {
    let Value::Object(object) = item else {
        return refuse(at, "a constraint is a JSON object");
    };
    closed(object, &["leftOperand", "operator", "rightOperand"], at)?;
    let text = |key: &str| object.get(key).and_then(Value::as_str);
    match (
        text("leftOperand"),
        text("operator"),
        object.get("rightOperand"),
    ) {
        (Some(l), Some(o), Some(r)) => Ok((l, o, r)),
        _ => refuse(
            at,
            "a constraint has leftOperand, operator and rightOperand",
        ),
    }
}

fn list<'a>(value: Option<&'a Value>, at: &str) -> Out<&'a [Value]> {
    match value {
        None => Ok(&[]),
        Some(Value::Array(items)) => Ok(items),
        Some(_) => refuse(at, "must be a list"),
    }
}

/// Sets `slot` once from a string operand.
fn once(slot: &mut Option<String>, right: &Value, at: &str) -> Out<()> {
    let Some(text) = right.as_str() else {
        return refuse(format!("{at}.rightOperand"), "must be a string");
    };
    if slot.replace(text.to_owned()).is_some() {
        return refuse(at, "this left operand is given twice in one rule");
    }
    Ok(())
}

/// The rule's constraints into `grant`: residual filters, the decomposed `geoQ` and `dateTime`
/// the broker writes, and the validity window.
fn filters(grant: &mut Grant, value: Option<&Value>, at: &str) -> Out<()> {
    let (mut after, mut before) = (None, None);
    for (n, item) in list(value, at)?.iter().enumerate() {
        let at = format!("{at}[{n}]");
        let (left, operator, right) = triple(item, &at)?;
        match (left, operator) {
            ("ngsi-ld:q", "eq") => once(&mut grant.q, right, &at)?,
            ("ngsi-ld:scopeQ", "isPartOf") => once(&mut grant.scope_q, right, &at)?,
            ("ngsi-ld:geoQ", "eq") => once(&mut grant.geo_q, right, &at)?,
            ("ngsi-ld:geoQ", relation) => {
                once(&mut grant.geo_q, &json!(geo_q(relation, right, &at)?), &at)?
            }
            ("ngsi-ld:temporalQ", "eq") => once(&mut grant.temporal_q, right, &at)?,
            ("dateTime", "gteq") => once(&mut after, right, &at)?,
            ("dateTime", "lteq") => once(&mut before, right, &at)?,
            ("ngsi-ld:validity", "gteq" | "lt") => validity(grant, operator, right, &at)?,
            (
                "ngsi-ld:q" | "ngsi-ld:scopeQ" | "ngsi-ld:temporalQ" | "dateTime"
                | "ngsi-ld:validity",
                _,
            ) => {
                return refuse(
                    format!("{at}.operator"),
                    format!("{left} does not take {operator}"),
                )
            }
            _ => {
                return refuse(
                    format!("{at}.leftOperand"),
                    format!("unknown leftOperand {left}"),
                )
            }
        }
    }
    match temporal(after, before) {
        Some(temporal) => once(&mut grant.temporal_q, &json!(temporal), at),
        None => Ok(()),
    }
}

/// The broker's `dateTime` bounds as the `temporalQ` they stand for.
fn temporal(after: Option<String>, before: Option<String>) -> Option<String> {
    match (after, before) {
        (Some(a), Some(b)) => Some(format!("timerel=between;timeAt={a};endTimeAt={b}")),
        (Some(a), None) => Some(format!("timerel=after;timeAt={a}")),
        (None, Some(b)) => Some(format!("timerel=before;timeAt={b}")),
        (None, None) => None,
    }
}

/// The broker's decomposed `geoQ` (`ngsi-ld:within` over a GeoJSON geometry) as the query string.
fn geo_q(relation: &str, right: &Value, at: &str) -> Out<String> {
    const RELATIONS: [&str; 6] = [
        "within",
        "contains",
        "intersects",
        "disjoint",
        "equals",
        "overlaps",
    ];
    let Some(relation) = relation
        .strip_prefix("ngsi-ld:")
        .filter(|r| RELATIONS.contains(r))
    else {
        return refuse(
            format!("{at}.operator"),
            format!("ngsi-ld:geoQ takes eq or one of ngsi-ld:{RELATIONS:?}"),
        );
    };
    let geometry = right
        .get("@type")
        .and_then(Value::as_str)
        .and_then(|t| t.strip_prefix("geojson:"));
    match (
        right.as_object().map(Map::len),
        geometry,
        right.get("coordinates"),
    ) {
        (Some(2), Some(geometry), Some(Value::Array(c))) => Ok(format!(
            "georel={relation};geometry={geometry};coordinates={}",
            Value::Array(c.clone())
        )),
        _ => refuse(
            format!("{at}.rightOperand"),
            "a geometry is {\"@type\": \"geojson:Polygon\", \"coordinates\": [...]}",
        ),
    }
}

fn validity(grant: &mut Grant, operator: &str, right: &Value, at: &str) -> Out<()> {
    let Some(at_time) = right
        .as_str()
        .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
        .map(|t| t.with_timezone(&Utc))
    else {
        return refuse(
            format!("{at}.rightOperand"),
            "an RFC 3339 time, e.g. 2027-01-01T00:00:00Z",
        );
    };
    let window = grant.validity.get_or_insert(Validity {
        from: None,
        to: None,
    });
    let slot = if operator == "gteq" {
        &mut window.from
    } else {
        &mut window.to
    };
    if slot.replace(at_time).is_some() {
        return refuse(at, "this validity bound is given twice in one rule");
    }
    Ok(())
}

/// A target: one `ngsi-ld:EntityType` with its id and attribute refinements.
fn asset(value: &Value, at: &str) -> Out<Target> {
    let Value::Object(object) = value else {
        return refuse(
            at,
            "a target is {\"@type\": \"ngsi-ld:EntityType\", \"uid\": \"<type>\"}",
        );
    };
    closed(object, &["@type", "uid", "refinement", "partOf"], at)?;
    if object.get("@type").and_then(Value::as_str) != Some("ngsi-ld:EntityType") {
        return refuse(format!("{at}.@type"), "must be ngsi-ld:EntityType");
    }
    let Some(entity_type) = object.get("uid").and_then(Value::as_str) else {
        return refuse(format!("{at}.uid"), "name the Entity type");
    };
    let part = match object.get("partOf") {
        None => None,
        Some(Value::String(part)) => Some(part.clone()),
        Some(_) => {
            let reason = "names the information entry, e.g. \"#information-1\"";
            return refuse(format!("{at}.partOf"), reason);
        }
    };
    let mut refined = Refined::default();
    let at = format!("{at}.refinement");
    for (n, item) in list(object.get("refinement"), &at)?.iter().enumerate() {
        refine(&mut refined, item, &format!("{at}[{n}]"))?;
    }
    let selector = EntitySelector {
        entity_type: entity_type.to_owned(),
        id: refined.id,
        id_pattern: refined.id_pattern,
    };
    let names = |n: Option<Vec<String>>| n.unwrap_or_default();
    Ok((
        selector,
        names(refined.properties),
        names(refined.relationships),
        part,
    ))
}

/// What a target's refinements say.
#[derive(Default)]
struct Refined {
    id: Option<Urn>,
    id_pattern: Option<String>,
    properties: Option<Vec<String>>,
    relationships: Option<Vec<String>>,
}

/// One refinement into `refined`; each left operand once.
fn refine(refined: &mut Refined, item: &Value, at: &str) -> Out<()> {
    let (left, operator, right) = triple(item, at)?;
    let names = || -> Out<Vec<String>> {
        serde_json::from_value(right.clone()).map_err(|_| Refusal {
            member: format!("{at}.rightOperand"),
            reason: "a list of attribute names".to_owned(),
        })
    };
    let given_twice = match (left, operator) {
        ("ngsi-ld:id", "eq") => {
            let Some(urn) = right.as_str().and_then(|u| u.parse().ok()) else {
                let reason = "an Entity URN, e.g. urn:ngsi-ld:Sensor:1";
                return refuse(format!("{at}.rightOperand"), reason);
            };
            refined.id.replace(urn).is_some()
        }
        ("ngsi-ld:idPattern", "eq") => return once(&mut refined.id_pattern, right, at),
        ("ngsi-ld:attrs", "isAnyOf") => {
            let names = names()?;
            let first = refined.properties.replace(names.clone()).is_some();
            refined.relationships.replace(names).is_some() || first
        }
        ("ngsi-ld:propertyNames", "isAnyOf") => refined.properties.replace(names()?).is_some(),
        ("ngsi-ld:relationshipNames", "isAnyOf") => {
            refined.relationships.replace(names()?).is_some()
        }
        (
            "ngsi-ld:id"
            | "ngsi-ld:idPattern"
            | "ngsi-ld:attrs"
            | "ngsi-ld:propertyNames"
            | "ngsi-ld:relationshipNames",
            _,
        ) => {
            let reason = format!("{left} does not take {operator}");
            return refuse(format!("{at}.operator"), reason);
        }
        _ => {
            return refuse(
                format!("{at}.leftOperand"),
                format!("unknown leftOperand {left}"),
            )
        }
    };
    if given_twice {
        return refuse(at, "this left operand is given twice in one target");
    }
    Ok(())
}

/// Consecutive ODRL rules with one grant become one Policy rule, consecutive selectors with the
/// same names one `information` entry: the inverse of [`render`].
fn regroup(parsed: Vec<(Grant, Option<Target>)>) -> Vec<(Grant, Vec<RegistrationInfo>)> {
    let mut grants: Vec<(Grant, Vec<RegistrationInfo>, Option<String>)> = Vec::new();
    for (grant, target) in parsed {
        match (grants.last_mut(), target) {
            (Some((last, infos, part)), Some(target)) if *last == grant && !infos.is_empty() => {
                add(infos, part, target)
            }
            (_, target) => {
                let (mut infos, mut part) = (Vec::new(), None);
                if let Some(target) = target {
                    add(&mut infos, &mut part, target);
                }
                grants.push((grant, infos, part));
            }
        }
    }
    grants.into_iter().map(|(g, infos, _)| (g, infos)).collect()
}

/// `target` joins the last entry when its names and `partOf` are that entry's.
fn add(
    infos: &mut Vec<RegistrationInfo>,
    last_part: &mut Option<String>,
    (selector, properties, relationships, part): Target,
) {
    let same_part = *last_part == part;
    *last_part = part;
    match infos.last_mut() {
        Some(info)
            if same_part
                && info.property_names == properties
                && info.relationship_names == relationships =>
        {
            info.entities.push(selector)
        }
        _ => infos.push(RegistrationInfo {
            entities: vec![selector],
            property_names: properties,
            relationship_names: relationships,
        }),
    }
}

/// The single-rule shape for one rule, `rules[]` for several.
fn spec_of(
    grants: Vec<(Grant, Vec<RegistrationInfo>)>,
    assigner: String,
    space: &str,
    validity: Option<Validity>,
) -> PolicySpec {
    let mut rules: Vec<PolicyRule> = grants
        .into_iter()
        .map(|(g, information)| PolicyRule {
            effect: g.effect,
            assignee: g.assignee,
            operations: g.operations,
            actions: g.actions,
            information,
            q: g.q,
            scope_q: g.scope_q,
            geo_q: g.geo_q,
            temporal_q: g.temporal_q,
        })
        .collect();
    let mut spec = PolicySpec {
        context_space_ref: Ref::Typed(TypedRef {
            kind: "ContextSpace".to_owned(),
            name: space.to_owned(),
            namespace: None,
        }),
        effect: PolicyEffect::Permission,
        assigner,
        assignee: Principal::unset(),
        operations: Vec::new(),
        actions: Vec::new(),
        information: Vec::new(),
        q: None,
        scope_q: None,
        geo_q: None,
        temporal_q: None,
        rules: None,
        validity,
    };
    match (rules.len(), rules.pop()) {
        (1, Some(only)) => {
            spec.effect = only.effect;
            spec.assignee = only.assignee;
            spec.operations = only.operations;
            spec.actions = only.actions;
            spec.information = only.information;
            spec.q = only.q;
            spec.scope_q = only.scope_q;
            spec.geo_q = only.geo_q;
            spec.temporal_q = only.temporal_q;
        }
        (_, last) => {
            rules.extend(last);
            spec.rules = Some(rules);
        }
    }
    spec
}

#[cfg(test)]
mod tests {
    use super::*;

    const POLICY: &str = "apiVersion: joinedcontext.com/v1alpha1\nkind: Policy\nmetadata: { name: air, namespace: ovzdusie }\nspec:\n  contextSpaceRef: { kind: ContextSpace, name: ovzdusie }\n  assigner: did:web:banskabystrica.sk\n  assignee: { kind: role, id: public }\n  operations: [queryEntity]\n  information: [{ entities: [{ type: AirQualityObserved }] }]\n  temporalQ: timerel=after;timeAt=P-1D\n";

    #[test]
    fn render_then_import_is_the_manifest() -> Out<()> {
        let policy = Policy::from_yaml(POLICY).map_err(|e| Refusal {
            member: "fixture".into(),
            reason: e.to_string(),
        })?;
        let set = render(&policy)?;
        assert_eq!(set["uid"], "urn:joinedcontext:policy:ovzdusie:ovzdusie:air");
        assert_eq!(import(&set, &Names::default())?, policy);
        Ok(())
    }

    #[test]
    fn a_duty_is_refused_not_dropped() {
        let set = json!({"@context": CONTEXTS, "@type": "Set", "assigner": "did:web:x",
            "permission": [{"assignee": "did:web:y", "action": ["ngsi-ld:queryEntity"],
                            "duty": [{"action": "attribute"}]}]});
        let names = Names {
            project: Some("p".into()),
            space: Some("s".into()),
            name: Some("n".into()),
        };
        let refusal = import(&set, &names).err();
        assert_eq!(
            refusal.map(|r| r.member).as_deref(),
            Some("permission[0].duty")
        );
    }

    #[test]
    fn the_brokers_date_bounds_are_a_temporal_query() {
        let at = |t: &str| Some(t.to_owned());
        assert_eq!(
            temporal(at("A"), None).as_deref(),
            Some("timerel=after;timeAt=A")
        );
        assert_eq!(
            temporal(None, at("B")).as_deref(),
            Some("timerel=before;timeAt=B")
        );
        assert_eq!(temporal(None, None), None);
    }
}
