//! The metadata members a caller's grants pass for each attribute (ADR-N-066 2.4, DM-139,
//! DM-151): one shared function decides them, refuses a query that filters or orders by a
//! member it strips, and strips the rest from an answer.
//!
//! A grant without an `attributes` block keeps today's answer: every member, except that a
//! grant to `role: public` never passes `modifiedBy.actor` or a version's whole `provenance`
//! (DM-140). A grant with a block passes, per attribute, the members its entry lists, else the
//! `*` entry's, else the audience's defaults; members are united across the caller's grants
//! and a prohibition's listed members are taken back. ponytail: names are untyped across one
//! caller's grants, as the versions list is (T-6338); per-type sets are the upgrade.

use crate::auth::subject::Subject;
use crate::pdp::evaluator;
use chrono::{DateTime, Utc};
use jc_core::kinds::policy::{MetadataMember, PrincipalKind};
use jc_core::kinds::{Operation, PolicySpec};
use jc_core::ProblemDetails;
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};

type Members = BTreeSet<MetadataMember>;

use MetadataMember::{
    CreatedAt, ModifiedAt, ModifiedByActor, ModifiedByActorKind, ModifiedByOrganization,
    ObservedAt, Provenance, Reason, UnitCode,
};

const ALL: [MetadataMember; 9] = [
    ObservedAt,
    UnitCode,
    CreatedAt,
    ModifiedAt,
    ModifiedByOrganization,
    ModifiedByActorKind,
    ModifiedByActor,
    Provenance,
    Reason,
];

/// The members the broker sends only to a caller holding `readVersions`: `modifiedBy` and a
/// version's provenance (ADR-N-054 2.8).
const WITH_VERSIONS: [MetadataMember; 5] = [
    ModifiedByOrganization,
    ModifiedByActorKind,
    ModifiedByActor,
    Provenance,
    Reason,
];

/// The provenance members a version answer keeps without the `provenance` member (DM-151).
const VERSION_PROVENANCE: [&str; 4] = ["writtenAt", "operation", "organization", "actorKind"];

/// The keys of an entity that are not attributes.
const NOT_ATTRIBUTES: [&str; 7] = [
    "id",
    "type",
    "@context",
    "scope",
    "createdAt",
    "modifiedAt",
    "deletedAt",
];

/// One grant's members: per attribute its block names, and for every other attribute.
#[derive(Debug)]
struct Grant {
    named: BTreeMap<String, Members>,
    rest: Members,
}

impl Grant {
    fn members(&self, attribute: &str) -> &Members {
        self.named.get(attribute).unwrap_or(&self.rest)
    }
}

/// What the caller's grants pass, when they narrow an answer at all.
#[derive(Debug)]
pub struct Passed {
    grants: Vec<Grant>,
    taken: Vec<Grant>,
}

fn public(policy: &PolicySpec) -> bool {
    policy.assignee.kind == PrincipalKind::Role && policy.assignee.id == "public"
}

/// The audience's defaults (ADR-N-066 2.4).
fn defaults(public: bool) -> Members {
    let mut members = Members::from([ObservedAt, UnitCode, ModifiedAt]);
    if !public {
        members.extend([ModifiedByOrganization, ModifiedByActorKind]);
    }
    members
}

/// A permission's members.
fn granted(policy: &PolicySpec) -> Grant {
    let public = public(policy);
    let mut named = BTreeMap::new();
    let mut star = None;
    for info in &policy.information {
        for (name, caps) in &info.attributes {
            if let Some(listed) = &caps.metadata {
                let listed: Members = listed.iter().copied().collect();
                if name == "*" {
                    star.get_or_insert_with(Members::new).extend(listed);
                } else {
                    named
                        .entry(name.clone())
                        .or_insert_with(Members::new)
                        .extend(listed);
                }
            }
        }
    }
    let blocked = policy.information.iter().any(|i| !i.attributes.is_empty());
    let rest = match (blocked, star) {
        (_, Some(star)) => star,
        (true, None) => defaults(public),
        (false, None) => ALL.into_iter().collect(),
    };
    let mut grant = Grant { named, rest };
    if public {
        // DM-140: never public, whatever a block lists (jc-core refuses such a block too).
        for members in grant.named.values_mut().chain([&mut grant.rest]) {
            members.remove(&ModifiedByActor);
            members.remove(&Provenance);
        }
    }
    grant
}

/// A prohibition's members: only those its block lists.
fn prohibited(policy: &PolicySpec) -> Grant {
    let mut grant = Grant {
        named: BTreeMap::new(),
        rest: Members::new(),
    };
    for info in &policy.information {
        for (name, caps) in &info.attributes {
            let listed = caps.metadata.iter().flatten().copied();
            if name == "*" {
                grant.rest.extend(listed);
            } else {
                grant.named.entry(name.clone()).or_default().extend(listed);
            }
        }
    }
    grant
}

impl Passed {
    /// What `subject`'s grants of `operation` pass now; `None` when they pass every member the
    /// broker sends, which leaves the answer as it is. `versions` is whether the broker sends
    /// `modifiedBy` and provenance at all (the caller holds `readVersions`).
    pub fn of(
        subject: &Subject,
        policies: &[PolicySpec],
        operation: Operation,
        versions: bool,
        now: DateTime<Utc>,
    ) -> Option<Self> {
        let applying = || {
            policies
                .iter()
                .filter(move |p| evaluator::applies(p, subject, operation, now))
        };
        let passed = Self {
            grants: applying()
                .filter(|p| !p.effect.is_prohibition())
                .map(granted)
                .collect(),
            taken: applying()
                .filter(|p| p.effect.is_prohibition())
                .map(prohibited)
                .collect(),
        };
        let sent: Members = ALL
            .into_iter()
            .filter(|m| versions || !WITH_VERSIONS.contains(m))
            .collect();
        let narrows = passed
            .names()
            .into_iter()
            .any(|name| !sent.is_subset(&passed.members(name)));
        narrows.then_some(passed)
    }

    /// Every attribute name a block names, and `*` for the others.
    fn names(&self) -> BTreeSet<&str> {
        self.grants
            .iter()
            .chain(&self.taken)
            .flat_map(|g| g.named.keys().map(String::as_str))
            .chain(["*"])
            .collect()
    }

    /// The members passed for `attribute` (`*`: one no block names).
    pub fn members(&self, attribute: &str) -> Members {
        let mut members: Members = self
            .grants
            .iter()
            .flat_map(|g| g.members(attribute).iter().copied())
            .collect();
        for taken in &self.taken {
            members.retain(|m| !taken.members(attribute).contains(m));
        }
        members
    }

    /// The `400` a query earns that filters or orders by a member stripped for its attribute
    /// (DM-139): `q`, `orderBy` and a temporal `timeproperty`.
    pub fn refusal(&self, params: &[(String, String)]) -> Option<ProblemDetails> {
        for (name, value) in params {
            let named = match name.as_str() {
                "q" | "orderBy" => named_members(value),
                "timeproperty" => match value.as_str() {
                    "observedAt" => vec![("*".to_owned(), ObservedAt, "observedAt")],
                    "createdAt" => vec![("*".to_owned(), CreatedAt, "createdAt")],
                    "modifiedAt" => vec![("*".to_owned(), ModifiedAt, "modifiedAt")],
                    _ => Vec::new(),
                },
                _ => Vec::new(),
            };
            for (attribute, member, word) in named {
                let stripped = if attribute == "*" {
                    !self.everywhere(member)
                } else {
                    !self.members(&attribute).contains(&member)
                };
                if stripped {
                    let what = if attribute == "*" {
                        format!("`{word}`")
                    } else {
                        format!("`{attribute}.{word}`")
                    };
                    return Some(ProblemDetails::bad_request().with_detail(format!(
                        "{name} names {what}, a metadata member your access does not pass; \
                         ask without it"
                    )));
                }
            }
        }
        None
    }

    /// Whether `member` passes for every attribute, the unnamed ones included.
    fn everywhere(&self, member: MetadataMember) -> bool {
        self.names()
            .into_iter()
            .all(|name| self.members(name).contains(&member))
    }

    /// Strips every member not passed from an NGSI-LD answer: one entity, a list of them, their
    /// temporal evolutions with versions, or a GeoJSON feature collection.
    pub fn strip(&self, body: &mut Value) {
        match body {
            Value::Array(items) => items.iter_mut().for_each(|item| self.strip(item)),
            Value::Object(object) => match object.get_mut("features") {
                Some(Value::Array(features)) => {
                    for feature in features {
                        if let Some(Value::Object(properties)) = feature.get_mut("properties") {
                            self.strip_entity(properties);
                        }
                    }
                }
                _ => self.strip_entity(object),
            },
            _ => {}
        }
    }

    fn strip_entity(&self, entity: &mut Map<String, Value>) {
        entity.retain(|name, value| {
            if NOT_ATTRIBUTES.contains(&name.as_str()) {
                return true;
            }
            let members = self.members(name);
            match value {
                Value::Object(instance) => strip_instance(&members, instance),
                Value::Array(instances) => {
                    instances.retain_mut(|instance| match instance {
                        Value::Object(instance) => strip_instance(&members, instance),
                        _ => true,
                    });
                    true
                }
                _ => true,
            }
        });
    }
}

/// Strips one attribute instance; `false` drops it whole: a temporal `values` list whose times
/// are a member the caller may not read.
fn strip_instance(members: &Members, instance: &mut Map<String, Value>) -> bool {
    if instance.contains_key("values") && !members.contains(&ObservedAt) {
        return false;
    }
    for (member, key) in [
        (ObservedAt, "observedAt"),
        (UnitCode, "unitCode"),
        (CreatedAt, "createdAt"),
        (ModifiedAt, "modifiedAt"),
    ] {
        if !members.contains(&member) {
            instance.remove(key);
        }
    }
    if let Some(Value::Object(by)) = instance.get_mut("modifiedBy") {
        for (member, key) in [
            (ModifiedByOrganization, "organization"),
            (ModifiedByActorKind, "actorKind"),
            (ModifiedByActor, "actor"),
        ] {
            if !members.contains(&member) {
                by.remove(key);
            }
        }
        if by.is_empty() {
            instance.remove("modifiedBy");
        }
    }
    if !members.contains(&Provenance) {
        if let Some(Value::Object(provenance)) = instance.get_mut("provenance") {
            provenance.retain(|key, _| {
                VERSION_PROVENANCE.contains(&key.as_str())
                    || (key == "reason" && members.contains(&Reason))
            });
        }
    }
    true
}

/// The `(attribute, member, word)` triples a `q` or `orderBy` names as `attr.member`.
fn named_members(text: &str) -> Vec<(String, MetadataMember, &'static str)> {
    let mut found = Vec::new();
    let words = text.split(|c: char| {
        matches!(
            c,
            ';' | '|' | '(' | ')' | '!' | '=' | '<' | '>' | '~' | ',' | '"'
        ) || c.is_whitespace()
    });
    for word in words {
        let path: Vec<&str> = word.split('.').collect();
        let [attribute, member, rest @ ..] = path.as_slice() else {
            continue;
        };
        let attribute = attribute.split('[').next().unwrap_or_default();
        let member = match (*member, rest.first().copied()) {
            ("observedAt", _) => (ObservedAt, "observedAt"),
            ("unitCode", _) => (UnitCode, "unitCode"),
            ("createdAt", _) => (CreatedAt, "createdAt"),
            ("modifiedAt", _) => (ModifiedAt, "modifiedAt"),
            ("modifiedBy", Some("organization")) => {
                (ModifiedByOrganization, "modifiedBy.organization")
            }
            ("modifiedBy", Some("actorKind")) => (ModifiedByActorKind, "modifiedBy.actorKind"),
            ("modifiedBy", _) => (ModifiedByActor, "modifiedBy.actor"),
            _ => continue,
        };
        if !attribute.is_empty() {
            found.push((attribute.to_owned(), member.0, member.1));
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn policy(yaml: &str) -> PolicySpec {
        serde_norway::from_str(&format!(
            "contextSpaceRef: ovzdusie\nassigner: did:web:banskabystrica.sk\n{yaml}"
        ))
        .expect("the policy spec parses")
    }

    fn passed(policies: &[PolicySpec], subject: &Subject, versions: bool) -> Option<Passed> {
        Passed::of(
            subject,
            policies,
            Operation::QueryTemporal,
            versions,
            crate::pdp::now(),
        )
    }

    fn public_with(attributes: &str) -> PolicySpec {
        policy(&format!(
            "assignee: {{ kind: role, id: public }}\n\
             operations: [queryEntity, queryTemporal]\nactions: [readVersions]\n\
             information:\n- entities: [{{ type: AirQualityObserved }}]\n  attributes:\n{attributes}"
        ))
    }

    fn instance() -> Value {
        json!({
            "type": "Property", "value": 41, "observedAt": "2026-11-01T10:00:00Z",
            "unitCode": "GQ", "createdAt": "2026-10-01T00:00:00Z",
            "modifiedAt": "2026-11-01T10:00:01Z", "instanceId": "urn:ngsi-ld:i:1",
            "supersededAt": "2026-11-01T11:00:00Z",
            "modifiedBy": { "organization": "city", "actorKind": "person", "actor": "0f5a" },
            "provenance": {
                "writtenAt": "2026-11-01T10:00:01Z", "operation": "update",
                "organization": "city", "actorKind": "person", "actor": "0f5a",
                "reason": "Sensor stuck, checked by Jana", "channel": "ui", "requestId": "r1"
            }
        })
    }

    fn entity() -> Value {
        json!({ "id": "urn:ngsi-ld:AirQualityObserved:1", "type": "AirQualityObserved",
                "pm10": [instance()], "name": instance() })
    }

    #[test]
    fn a_grant_without_a_block_narrows_nothing() {
        let steward = policy(
            "assignee: { kind: role, id: steward }\noperations: [queryTemporal]\n\
             actions: [readVersions]\n",
        );
        let subject = Subject {
            roles: ["steward".to_owned()].into(),
            ..Subject::default()
        };
        assert!(passed(&[steward], &subject, true).is_none());
    }

    #[test]
    fn a_public_grant_without_a_block_still_strips_the_actor() {
        let public = policy(
            "assignee: { kind: role, id: public }\noperations: [queryTemporal]\n\
             actions: [readVersions]\n",
        );
        let p =
            passed(std::slice::from_ref(&public), &Subject::anonymous(), true).expect("narrows");
        let mut body = entity();
        p.strip(&mut body);
        assert!(body["name"]["modifiedBy"].get("actor").is_none(), "{body}");
        assert!(body["name"]["provenance"].get("actor").is_none(), "{body}");
        assert_eq!(
            body["name"]["provenance"]["reason"],
            "Sensor stuck, checked by Jana"
        );
        // Without readVersions the broker sends neither, so nothing is narrowed.
        assert!(passed(&[public], &Subject::anonymous(), false).is_none());
    }

    #[test]
    fn each_member_is_stripped_unless_listed() {
        let cases = [
            ("observedAt", "/observedAt"),
            ("unitCode", "/unitCode"),
            ("createdAt", "/createdAt"),
            ("modifiedAt", "/modifiedAt"),
            ("modifiedBy.organization", "/modifiedBy/organization"),
            ("modifiedBy.actorKind", "/modifiedBy/actorKind"),
        ];
        for (member, pointer) in cases {
            let p = passed(
                &[public_with(&format!(
                    "    pm10: {{ metadata: [{member}] }}\n"
                ))],
                &Subject::anonymous(),
                true,
            )
            .expect("narrows");
            let mut body = entity();
            p.strip(&mut body);
            let pm10 = &body["pm10"][0];
            assert!(pm10.pointer(pointer).is_some(), "{member} passed: {pm10}");
            for (other, other_pointer) in cases {
                if other != member {
                    assert!(
                        pm10.pointer(other_pointer).is_none(),
                        "{other} stripped: {pm10}"
                    );
                }
            }
            assert_eq!(pm10["value"], 41, "the value stays");
        }
    }

    #[test]
    fn an_unnamed_attribute_takes_the_audience_defaults() {
        let p = passed(
            &[public_with("    pm10: { metadata: [unitCode] }\n")],
            &Subject::anonymous(),
            true,
        )
        .expect("narrows");
        assert_eq!(
            p.members("name"),
            Members::from([ObservedAt, UnitCode, ModifiedAt])
        );
        let organization = policy(
            "assignee: { kind: role, id: steward }\noperations: [queryTemporal]\n\
             information:\n- entities: [{ type: AirQualityObserved }]\n  attributes:\n    \
             pm10: { history: true }\n",
        );
        let subject = Subject {
            roles: ["steward".to_owned()].into(),
            ..Subject::default()
        };
        let p = passed(&[organization], &subject, true).expect("narrows");
        assert_eq!(
            p.members("pm10"),
            Members::from([
                ObservedAt,
                UnitCode,
                ModifiedAt,
                ModifiedByOrganization,
                ModifiedByActorKind
            ])
        );
    }

    #[test]
    fn a_public_version_answer_carries_no_actor_and_a_reason_only_where_listed() {
        let p = passed(
            &[public_with(
                "    pm10: { metadata: [observedAt, reason] }\n    '*': { metadata: [observedAt] }\n",
            )],
            &Subject::anonymous(),
            true,
        )
        .expect("narrows");
        let mut body = entity();
        p.strip(&mut body);
        let kept = |v: &Value| {
            let mut keys: Vec<String> = v["provenance"]
                .as_object()
                .map(|o| o.keys().cloned().collect())
                .unwrap_or_default();
            keys.sort();
            keys
        };
        assert_eq!(
            kept(&body["pm10"][0]),
            [
                "actorKind",
                "operation",
                "organization",
                "reason",
                "writtenAt"
            ]
        );
        assert_eq!(
            kept(&body["name"]),
            ["actorKind", "operation", "organization", "writtenAt"]
        );
        for attribute in [&body["pm10"][0], &body["name"]] {
            assert!(attribute.get("modifiedBy").is_none(), "{attribute}");
            assert_eq!(attribute["instanceId"], "urn:ngsi-ld:i:1");
            assert_eq!(attribute["supersededAt"], "2026-11-01T11:00:00Z");
        }
    }

    #[test]
    fn a_query_on_a_stripped_member_is_refused_naming_it() {
        let p = passed(
            &[public_with("    pm10: { metadata: [observedAt] }\n")],
            &Subject::anonymous(),
            true,
        )
        .expect("narrows");
        let refused = |name: &str, value: &str| {
            p.refusal(&[(name.to_owned(), value.to_owned())])
                .and_then(|problem| problem.detail)
        };
        let detail = refused("q", "pm10>40;pm10.unitCode==\"GQ\"").expect("refused");
        assert!(detail.contains("`pm10.unitCode`"), "{detail}");
        assert!(refused("orderBy", "pm10.modifiedAt;desc").is_some());
        assert!(refused("q", "name.modifiedBy.actor==\"0f5a\"").is_some());
        assert!(refused("timeproperty", "createdAt").is_some());
        assert_eq!(refused("q", "pm10.observedAt>=2026-11-01T00:00:00Z"), None);
        assert_eq!(refused("q", "pm10>40"), None);
        assert_eq!(refused("timeproperty", "observedAt"), None);
    }

    #[test]
    fn a_prohibition_takes_back_what_it_lists() {
        let take = policy(
            "assignee: { kind: role, id: public }\neffect: prohibition\n\
             operations: [queryTemporal]\n\
             information:\n- entities: [{ type: AirQualityObserved }]\n  attributes:\n    \
             pm10: { metadata: [unitCode] }\n",
        );
        let grant = policy("assignee: { kind: role, id: public }\noperations: [queryTemporal]\n");
        let p = passed(&[grant, take], &Subject::anonymous(), false).expect("narrows");
        assert!(!p.members("pm10").contains(&UnitCode));
        assert!(p.members("name").contains(&UnitCode));
    }

    #[test]
    fn a_temporal_values_list_whose_times_are_stripped_is_dropped_and_geojson_is_narrowed() {
        let p = passed(
            &[public_with("    pm10: { metadata: [unitCode] }\n")],
            &Subject::anonymous(),
            false,
        )
        .expect("narrows");
        let mut values = json!({ "id": "x", "type": "T",
            "pm10": { "type": "Property", "values": [[41, "2026-11-01T10:00:00Z"]] } });
        p.strip(&mut values);
        assert!(values.get("pm10").is_none(), "{values}");
        let mut features = json!({ "type": "FeatureCollection", "features": [
            { "type": "Feature", "id": "x", "properties": { "pm10": instance() } }
        ]});
        p.strip(&mut features);
        let pm10 = &features["features"][0]["properties"]["pm10"];
        assert!(
            pm10.get("observedAt").is_none() && pm10.get("unitCode").is_some(),
            "{pm10}"
        );
    }
}
