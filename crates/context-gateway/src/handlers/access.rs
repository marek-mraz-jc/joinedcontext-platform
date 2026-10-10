//! What the caller may do, answered by the same PDP that enforces it (T-0163, EP-55…EP-60).
//!
//! The document is built from the policy set rather than from a request, so it answers
//! "what may I do here" without the caller having to probe for it. What it must never do
//! is answer "what exists here": a type no grant names is absent, not listed as denied
//! (EP-59, R20).

use crate::pdp::evaluator::{
    assigned_and_in_force, effective, granted_attrs, granted_operations, granted_types, Subject,
};
use crate::resolver::Endpoint;
use chrono::{DateTime, Utc};
use jc_core::kinds::PolicySpec;
use serde_json::{json, Map, Value};

/// The AuthZEN permissions document for one caller on one endpoint (EP-55).
pub fn permissions(subject: &Subject, endpoint: &Endpoint, now: DateTime<Utc>) -> Value {
    let (prohibitions, grants): (Vec<_>, Vec<_>) = effective(subject, &endpoint.policies, now)
        .into_iter()
        .partition(|policy| policy.effect.is_prohibition());

    let mut document = Map::new();
    document.insert("subject".to_owned(), subject_of(subject));
    document.insert(
        "resource".to_owned(),
        json!({
            "type": "endpoint",
            "id": endpoint.slug,
            "space": endpoint.space,
        }),
    );
    document.insert("permissions".to_owned(), json!(entries(&grants)));
    document.insert("prohibitions".to_owned(), json!(entries(&prohibitions)));
    // The Endpoint's own rate, so a caller learns it before it hits it (EP-56). It is a
    // property of the Endpoint, not of the caller's grants: every caller of this Endpoint
    // meets the same limit, and an Endpoint without one says nothing.
    if let Some(limits) = &endpoint.rate_limit {
        let mut named = Map::new();
        named.insert(
            "requestsPerMinute".to_owned(),
            json!(limits.requests_per_minute),
        );
        if let Some(burst) = limits.burst {
            named.insert("burst".to_owned(), json!(burst));
        }
        document.insert("limits".to_owned(), Value::Object(named));
    }
    Value::Object(document)
}

/// What decided one prospective request: the index into [`Endpoint::policies`] of the
/// Policy that did, so [`check`] and [`simulate`] read one decision and cannot disagree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// A permission covering the action and the type.
    Granted(usize),
    /// A prohibition covering the action, which ends it whatever any permission says (GW8).
    Prohibited(usize),
    /// Nothing grants it.
    NoGrant,
}

/// The decision on one prospective request, from the policies in force for `subject`.
pub fn decide(
    subject: &Subject,
    endpoint: &Endpoint,
    action: &str,
    entity_type: Option<&str>,
    now: DateTime<Utc>,
) -> Verdict {
    let applicable: Vec<(usize, &PolicySpec)> = endpoint
        .policies
        .iter()
        .enumerate()
        .filter(|(_, policy)| assigned_and_in_force(subject, policy, now))
        .collect();
    let covers = |policy: &PolicySpec| granted_operations(policy).contains(action);

    // GW8: a prohibition ends it, whatever any permission says.
    if let Some((index, _)) = applicable
        .iter()
        .find(|(_, policy)| policy.effect.is_prohibition() && covers(policy))
    {
        return Verdict::Prohibited(*index);
    }
    applicable
        .iter()
        .find(|(_, policy)| {
            !policy.effect.is_prohibition()
                && covers(policy)
                && entity_type
                    .is_none_or(|wanted| granted_types(&policy.information).contains(wanted))
        })
        .map_or(Verdict::NoGrant, |(index, _)| Verdict::Granted(*index))
}

/// One AuthZEN decision for one prospective request (R51).
///
/// A permitted decision may name who granted it: the caller holds that grant, so it is
/// theirs to see. A refusal names nothing, because the reason is the rule (GW6).
pub fn check(
    subject: &Subject,
    endpoint: &Endpoint,
    action: &str,
    entity_type: Option<&str>,
    now: DateTime<Utc>,
) -> Value {
    match decide(subject, endpoint, action, entity_type, now) {
        Verdict::Granted(index) => json!({
            "decision": true,
            "context": {
                "reason": "policy_grant_matched",
                "assigner": endpoint.policies[index].assigner,
            }
        }),
        Verdict::Prohibited(_) | Verdict::NoGrant => json!({ "decision": false }),
    }
}

/// The same decision for a subject the Portal names, with why (EP-103, T-3311).
///
/// Its reader is an administrator who may read every Policy, so unlike [`check`] it names the
/// Policy that decided a refusal too. `admitted` is the Endpoint's audience answer for the
/// subject (EP-14): a subject it refuses never reaches a Policy.
pub fn simulate(
    subject: &Subject,
    admitted: bool,
    endpoint: &Endpoint,
    action: &str,
    entity_type: Option<&str>,
    now: DateTime<Utc>,
) -> Value {
    if !admitted {
        return json!({ "decision": false, "context": { "reason": "not_admitted" } });
    }
    let named = |index: usize| {
        let mut context = json!({ "assigner": endpoint.policies[index].assigner });
        if let Some(name) = endpoint.policy_names.get(index) {
            context["policy"] = json!(name);
        }
        context
    };
    match decide(subject, endpoint, action, entity_type, now) {
        Verdict::Granted(index) => {
            let mut context = named(index);
            context["reason"] = json!("policy_grant_matched");
            json!({ "decision": true, "context": context })
        }
        Verdict::Prohibited(index) => {
            let mut context = named(index);
            context["reason"] = json!("prohibited");
            json!({ "decision": false, "context": context })
        }
        Verdict::NoGrant => json!({ "decision": false, "context": { "reason": "no_grant" } }),
    }
}

/// One entry per entity type a policy names, so the caller sees the shape of what it may
/// touch and nothing about the rest (EP-59).
fn entries(policies: &[&PolicySpec]) -> Vec<Value> {
    let mut entries = Vec::new();
    for policy in policies {
        let actions = json!(granted_operations(policy));
        let attributes = granted_attrs(&policy.information);
        // No whitelist is not an empty whitelist: the grant reaches every attribute of the
        // types it names.
        let attributes = if attributes.is_empty() {
            json!("*")
        } else {
            json!(attributes)
        };

        for selector in policy
            .information
            .iter()
            .flat_map(|info| info.entities.iter())
        {
            let mut resource = Map::new();
            resource.insert("type".to_owned(), json!(selector.entity_type));
            if let Some(pattern) = &selector.id_pattern {
                resource.insert("idPatterns".to_owned(), json!([pattern]));
            }
            if let Some(id) = &selector.id {
                resource.insert("id".to_owned(), json!(id.to_string()));
            }
            entries.push(json!({
                "resource": Value::Object(resource),
                "actions": actions,
                "attributes": attributes,
                "constraints": constraints(policy),
            }));
        }
    }
    entries
}

/// The residual the gateway would add to any request under this grant (GW11).
fn constraints(policy: &PolicySpec) -> Value {
    let mut out = Map::new();
    for (name, value) in [
        ("q", &policy.q),
        ("scopeQ", &policy.scope_q),
        ("geoQ", &policy.geo_q),
        ("temporalQ", &policy.temporal_q),
    ] {
        if let Some(value) = value {
            out.insert(name.to_owned(), json!(value));
        }
    }
    Value::Object(out)
}

/// The caller, as the document names them. Never more than the caller already knows.
fn subject_of(subject: &Subject) -> Value {
    match (&subject.user, &subject.service_account) {
        (Some(user), _) => json!({ "type": "user", "id": user }),
        (_, Some(account)) => json!({ "type": "serviceAccount", "id": account }),
        _ => json!({ "type": "role", "id": "public" }),
    }
}
