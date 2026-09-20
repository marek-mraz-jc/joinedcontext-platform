//! The in-process policy decision point (ADR-N-003, gateway-firewall).

pub mod conditional;
pub mod evaluator;
pub mod geo;
pub mod projection;
pub mod reaper;
pub mod scope_folding;
pub mod temporal;
pub mod vocabulary;
pub mod write_guard;

use crate::resolver::Endpoint;
use chrono::{DateTime, Utc};
use evaluator::{Request, Subject, Verdict};
use jc_core::kinds::Operation;
use std::collections::{BTreeMap, BTreeSet};
use std::time::{SystemTime, UNIX_EPOCH};

/// What decides. Every request passes it before anything is forwarded (GW1, ADR-N-003).
///
/// A trait rather than a function so a test can assert what the enforcement point does
/// with a verdict without having to write the policy that produces it.
pub trait Pdp: Send + Sync + 'static {
    /// The verdict for one caller, one operation and one request on one endpoint.
    fn decide(
        &self,
        subject: &Subject,
        operation: Operation,
        request: &Request,
        endpoint: &Endpoint,
    ) -> Verdict;
}

/// The decision point the gateway runs: the endpoint's own policies, evaluated now.
#[derive(Debug, Clone, Copy, Default)]
pub struct PolicyPdp;

impl Pdp for PolicyPdp {
    fn decide(
        &self,
        subject: &Subject,
        operation: Operation,
        request: &Request,
        endpoint: &Endpoint,
    ) -> Verdict {
        let verdict = evaluator::evaluate(
            subject,
            operation,
            request,
            &endpoint.space,
            &endpoint.policies,
            now(),
        );
        // OPS-16: the one number a dashboard cannot get from the edge. APISIX counts requests
        // and statuses; only the decision point knows a request was answered because a policy
        // let it through rather than because nothing looked.
        crate::telemetry::decided(operation, verdict.is_deny());

        // EP-61: the endpoint's own publication narrowing, folded into the one decision
        // every representation reads, so no encoder can forget it and no new
        // representation has to remember it.
        let verdict = match verdict {
            Verdict::Rewrite(mut constraints) if !endpoint.hidden_attributes.is_empty() => {
                constraints.hidden = endpoint.hidden_attributes.clone();
                constraints.restricted = true;
                Verdict::Rewrite(constraints)
            }
            verdict => verdict,
        };

        // MP-02: the endpoint's projection, intersected into the same decision. Types and
        // attributes narrow like a grant's; the residual filter is conjoined like a REWRITE
        // constraint; nothing here can add what a policy did not give.
        let verdict = match (verdict, &endpoint.projection) {
            (Verdict::Rewrite(constraints), Some(projection)) => {
                project_constraints(*constraints, projection)
            }
            (verdict, _) => verdict,
        };

        // The filter is the last thing narrowed, because it is narrowed by what the two steps
        // above decided: a type whose attributes do not cover every name the request filters or
        // orders on leaves the query before the broker is asked (T-1862).
        match verdict {
            Verdict::Rewrite(constraints) => {
                drop_types_that_may_not_be_filtered(*constraints, request)
            }
            verdict => verdict,
        }
    }
}

/// The constraints of one decision, narrowed to a projection (MP-02).
///
/// A request type outside the projection is a refusal rather than an empty answer: an
/// empty `types` set means "no type filter" downstream, and the broker would be asked for
/// every type instead of none (the same rule the evaluator applies to grants, GW10).
fn project_constraints(
    mut constraints: evaluator::Constraints,
    projection: &jc_core::kinds::ModelProjectionSpec,
) -> Verdict {
    let classes: BTreeSet<String> = projection
        .classes
        .iter()
        .map(|class| class.name.clone())
        .collect();
    let types = evaluator::narrow(&constraints.types, &classes);
    if types.is_empty() {
        return Verdict::Deny;
    }
    // Per type, because a projection is a statement about a class: `User: [age]` and
    // `Vehicle: [weight]` joined into one set served a Vehicle's `age` to anyone who asked for
    // both types (T-1862, MP-02). The joined set stays as `attrs`, which is the one list CIM 009
    // lets the broker be told; the answer is stripped by the map.
    let by_type: BTreeMap<String, BTreeSet<String>> = types
        .iter()
        .map(|class| {
            let slots: BTreeSet<String> = projection.attributes_of(class).unwrap_or_default();
            (
                class.clone(),
                evaluator::narrow_to_identity(&constraints.attrs, &slots),
            )
        })
        .collect();
    let slots: BTreeSet<String> = types
        .iter()
        .filter_map(|class| projection.attributes_of(class))
        .flatten()
        .collect();
    constraints.attrs = evaluator::narrow_to_identity(&constraints.attrs, &slots);
    constraints.attrs_by_type = by_type;
    constraints.types = types;
    if let Some(filter) = &projection.filter {
        if let Some(q) = &filter.q {
            constraints.q = evaluator::conjoin(constraints.q.as_deref(), std::slice::from_ref(q));
        }
        // A filter the constraint set has no slot to conjoin with applies when the grants
        // set none; a grant's own geo or temporal clamp is narrower by construction (GW11).
        if constraints.geo_q.is_none() {
            constraints.geo_q = filter.geo_q.clone();
        }
        if constraints.temporal_q.is_none() {
            constraints.temporal_q = filter.temporal_q.clone();
        }
    }
    constraints.restricted = true;
    Verdict::Rewrite(Box::new(constraints))
}

/// The members every entity carries whatever the grants say, so filtering on one is not a
/// reference to anything a grant could withhold (CIM 009 4.5.1).
const ALWAYS_SERVED: &[&str] = &[
    "id",
    "type",
    "scope",
    "createdAt",
    "modifiedAt",
    "deletedAt",
    "expiresAt",
    "observedAt",
];

/// Takes out of the query every type that may not be filtered on what the request filters on
/// (T-1862; owner's rule of 2026-09-18, MP-02, R9).
///
/// A filter is a read. `q=age>30` over a type whose `age` this endpoint does not serve used to
/// reach the broker as written and the answer was stripped afterwards, so the rows that came back
/// still said which entities have an `age` over thirty — and bisecting `N` reads the value exactly.
/// The rule is therefore about which entities are *considered*, not about what the answer carries:
/// a type stays only if every referenced attribute is one it may serve.
///
/// Strict on purpose: a type is dropped even when the name it may not serve sits in one branch of
/// an `|`, because an `|` still lets the branch decide whether a row comes back. ponytail: strict
/// drop; splitting the query per type is the upgrade if people miss those rows.
///
/// No type left means the answer is genuinely nothing: `constraints.empty` is what the surfaces
/// already answer with `200 []` for a query and `404` for an addressed read, which is exactly what
/// an attribute that does not exist would give.
pub fn drop_types_that_may_not_be_filtered(
    mut constraints: evaluator::Constraints,
    request: &Request,
) -> Verdict {
    let referenced: Vec<&String> = request
        .referenced
        .iter()
        .filter(|name| !ALWAYS_SERVED.contains(&name.as_str()))
        .collect();
    if referenced.is_empty() {
        return Verdict::Rewrite(Box::new(constraints));
    }

    // Hidden is a denial over every type (EP-61): filtering on a hidden name can never be served,
    // whether or not a projection says which type owns it.
    if referenced
        .iter()
        .any(|name| constraints.hidden.contains(name.as_str()))
    {
        constraints.empty = true;
        constraints.restricted = true;
        return Verdict::Rewrite(Box::new(constraints));
    }

    if constraints.attrs_by_type.is_empty() {
        // No projection: the grants' own whitelist is the whole of the narrowing, and it is that
        // whitelist a filter is judged against — never `attrs`, which is what the caller asked
        // for. An empty whitelist is a grant over the whole entity, so nothing is referenced that
        // is not served.
        if !constraints.served.is_empty()
            && referenced
                .iter()
                .any(|name| !constraints.served.contains(name.as_str()))
        {
            constraints.empty = true;
            constraints.restricted = true;
        }
        return Verdict::Rewrite(Box::new(constraints));
    }

    let kept: BTreeSet<String> = constraints
        .attrs_by_type
        .iter()
        .filter(|(_, slots)| referenced.iter().all(|name| slots.contains(name.as_str())))
        .map(|(class, _)| class.clone())
        .collect();
    if kept.len() < constraints.attrs_by_type.len() {
        constraints.restricted = true;
        // Named, so the caller reads why a row is missing instead of narrowing the filter until
        // the empty answer tells them (T-1862, rule 4). Each of these is a type they may read.
        constraints.dropped = constraints
            .attrs_by_type
            .keys()
            .filter(|class| !kept.contains(*class))
            .cloned()
            .collect();
    }
    if kept.is_empty() {
        constraints.empty = true;
        return Verdict::Rewrite(Box::new(constraints));
    }
    constraints
        .attrs_by_type
        .retain(|class, _| kept.contains(class));
    constraints.types = kept;
    Verdict::Rewrite(Box::new(constraints))
}

/// The wall clock, as a `chrono` instant.
///
/// `jc-core` builds `chrono` without its `clock` feature so that manifest parsing cannot
/// depend on the time of day; the gateway does need the time of day, for policy validity
/// windows and key expiry, and reads it here in one place.
pub fn now() -> DateTime<Utc> {
    let since = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    DateTime::from_timestamp(since.as_secs() as i64, since.subsec_nanos()).unwrap_or_default()
}
