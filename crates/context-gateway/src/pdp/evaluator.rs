//! Turning a caller and a request into one verdict (T-0146, GW1..GW12).
//!
//! The evaluation is deliberately boring: prohibitions first, then permissions, and if
//! nothing matched the answer is DENY. Every path that cannot decide — an operation the
//! vocabulary does not know, a caller with no identity and no public grant, a policy that
//! is out of its validity window — ends in the same place, because a gateway that guesses
//! is a gateway that leaks (GW5, R5).
//!
//! A permission that matches does not produce ALLOW. It produces REWRITE with the union
//! of what the matching grants cover, which the caller's own request is then intersected
//! against (GW10). The floor for an ordinary caller is always REWRITE: the tenant alone is
//! pinned by the gateway, never chosen by the client (GW3, GW20).

use crate::pdp::temporal::{self, Window};
use crate::pdp::{geo, scope_folding};
use chrono::{DateTime, Utc};
use jc_core::kinds::{
    Operation, OperationRef, PolicySpec, Principal, PrincipalKind, RegistrationInfo,
};
use std::collections::{BTreeMap, BTreeSet};

/// Who is calling, as established by the PEP — never as claimed by the client (GW20).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Subject {
    /// The authenticated user, absent for an anonymous caller.
    pub user: Option<String>,
    /// The service account, when a workload is calling (PF-34).
    pub service_account: Option<String>,
    /// The groups the token asserts.
    pub groups: BTreeSet<String>,
    /// The roles the token asserts. An anonymous caller holds exactly `public` (GW22).
    pub roles: BTreeSet<String>,
    /// The decentralized identifier of a data-space participant, when one is calling.
    pub did: Option<String>,
    /// The data space agreement this caller acts under, which every audit record produced
    /// under it carries (DS-13). Never a grant of its own: what the agreement allows is in
    /// the `Policy` entities compiled from it.
    pub agreement: Option<String>,
    /// The service account a person's call came through, when a delegating client exchanged
    /// their token (ADR-N-038, AG-95). The audit line names it; it grants nothing, so it is
    /// never matched as a principal.
    pub via: Option<String>,
}

impl Subject {
    /// The anonymous caller: the role `public` and nothing else (GW22).
    pub fn anonymous() -> Self {
        Self {
            roles: BTreeSet::from(["public".to_owned()]),
            ..Self::default()
        }
    }

    /// Whether this policy's assignee is this caller.
    fn is(&self, assignee: &Principal) -> bool {
        let id = assignee.id.as_str();
        match assignee.kind {
            PrincipalKind::User => self.user.as_deref() == Some(id),
            PrincipalKind::Group => self.groups.contains(id),
            PrincipalKind::Role => self.roles.contains(id),
            PrincipalKind::ServiceAccount => self.service_account.as_deref() == Some(id),
            PrincipalKind::Did => self.did.as_deref() == Some(id),
        }
    }
}

/// What the caller asked for, in the dimensions a grant can narrow (GW11).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Request {
    /// The entity types asked for; empty means "whatever the grants cover".
    pub types: BTreeSet<String>,
    /// The attributes asked for; empty means "whatever the grants cover" (R9).
    pub attrs: BTreeSet<String>,
    /// The caller's own `q` filter.
    pub q: Option<String>,
    /// The caller's own `scopeQ` filter.
    pub scope_q: Option<String>,
    /// The caller's own `geoQ` filter.
    pub geo_q: Option<String>,
    /// The caller's own `temporalQ` filter.
    pub temporal_q: Option<String>,
    /// Every attribute this request uses to SELECT or ORDER entities (T-1862, owner's rule).
    ///
    /// A filter is an oracle: `q=age>30` over a type whose `age` the endpoint does not serve
    /// still answers with the entities that matched, and bisecting `N` reads the hidden value
    /// exactly. So the names are collected once, here, and a type that may not serve one of them
    /// leaves the query before the broker is asked.
    pub referenced: BTreeSet<String>,
}

/// The constraint set the gateway injects so the broker can physically only return or
/// accept what the grants cover (GW2, GW11).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Constraints {
    /// The tenant, pinned by the gateway from the endpoint (GW20).
    pub tenant: String,
    /// The entity types the request is narrowed to; empty means every granted type.
    pub types: BTreeSet<String>,
    /// The anchored id patterns of the matching grants (R24).
    pub id_patterns: BTreeSet<String>,
    /// The attributes the response is projected to; empty means no projection (R9).
    ///
    /// One set for the whole request, which is what CIM 009's own `attrs` parameter is and
    /// therefore all the broker can be told. It is a superset of what any one entity may carry
    /// whenever [`Self::attrs_by_type`] says which slots belong to which type.
    pub attrs: BTreeSet<String>,
    /// The attributes each type may serve, when a projection says so (MP-02, T-1862).
    ///
    /// `attrs` is the union over the effective types, because the broker takes one list. An
    /// answer stripped by that union serves a `Vehicle` the `age` the projection gives only to a
    /// `User`, so the stripping reads this map instead: an entity keeps the attributes of its own
    /// granted types, and a type this map does not name keeps its identity only. Empty means no
    /// projection narrowed the request and `attrs` is the whole of the narrowing.
    pub attrs_by_type: BTreeMap<String, BTreeSet<String>>,
    /// The attributes the grants themselves whitelist, before the caller's own `attrs` narrowed
    /// them (R9, T-1862).
    ///
    /// `attrs` is what the broker is told to return, which is the intersection with what the
    /// caller asked for; this is what the caller *may* be told about at all, and it is what a
    /// filter is judged against. Empty means no grant narrows the attributes.
    pub served: BTreeSet<String>,
    /// The attributes the endpoint publishes nothing of, whatever the grants say (EP-61).
    ///
    /// A denial rather than a second whitelist: `attrs` empty means "every granted
    /// attribute", and subtracting from an empty whitelist would say the opposite.
    pub hidden: BTreeSet<String>,
    /// The `q` sent to the broker: the caller's, AND the union of the per-policy filters,
    /// each of which already carries that policy's own scopes as an anchored regex
    /// (R12, R13).
    pub q: Option<String>,
    /// The scope tree the grants cover, which a write payload's own `scope` must sit
    /// inside (R29, R30).
    ///
    /// Never sent upstream and never carrying anything the caller wrote: a caller that
    /// could add a path here would be granting itself a district.
    pub granted_scopes: Option<String>,
    /// The `geoQ` sent to the broker.
    pub geo_q: Option<String>,
    /// Grant areas the answer is filtered against here, because the broker was given
    /// something wider; an entity must lie inside at least one (GW11).
    pub geo_grants: Vec<String>,
    /// Every granted area, whichever of them the broker was given (GW16, T-0807).
    ///
    /// What a write is decided against: a read may lean on the broker for the one area it
    /// forwarded, a write may not, because the broker is not told where the entity has to
    /// stay. Empty means no grant narrows space at all.
    pub geo_areas: Vec<String>,
    /// The caller's own area, when the broker was given the grant's instead (R14).
    pub geo_caller: Option<String>,
    /// The `temporalQ` sent to the broker.
    pub temporal_q: Option<String>,
    /// The windows an attribute instance must fall into, when the forwarded interval is
    /// the hull of several (GW26).
    pub temporal_windows: Vec<Window>,
    /// The caller asked for a period no grant reaches: the answer is an empty list rather
    /// than a refusal, and the broker is never called (GW26).
    pub empty: bool,
    /// Whether the caller asked for more than the grants cover, so the answer is a
    /// narrowed one and says so (R22).
    pub restricted: bool,
    /// The types this query was not allowed to select on, because it filters or orders on an
    /// attribute they may not serve (T-1862, MP-02).
    ///
    /// Every one of them is a type the caller may read — that is what makes it safe to name in a
    /// warning: what the answer cannot say is *why* a row is missing, and a caller reading an
    /// empty list with no explanation debugs it by asking the same question with more filters,
    /// which is the behaviour the rule exists to discourage.
    pub dropped: BTreeSet<String>,
}

impl Constraints {
    /// The same constraints written in the model the broker holds, for a view endpoint (DM-51).
    ///
    /// A grant on a view endpoint names the class the view serves; the broker's answer carries the
    /// class it stores. The query's `type` is already inverted before it is sent
    /// (`invert_query`), and the answer has to be judged by the same pair — otherwise the type
    /// check drops every entity a view endpoint ever returns, which is what T-2130 did until
    /// T-2241.
    pub fn in_source_model(&self, target_class: &str, source_class: &str) -> Self {
        let mut inverted = self.clone();
        if inverted.types.remove(target_class) {
            inverted.types.insert(source_class.to_owned());
        }
        inverted
    }
}

/// What the gateway does with the request (GW1, GW2, GW3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Refused. A single-entity read answers 404, anything else 403 (GW1, R20).
    Deny,
    /// Forwarded with the constraint set injected (GW2). The normal answer.
    Rewrite(Box<Constraints>),
}

impl Verdict {
    /// Whether the request is refused.
    pub fn is_deny(&self) -> bool {
        matches!(self, Verdict::Deny)
    }

    /// The constraints of a rewrite, `None` on a deny.
    pub fn constraints(&self) -> Option<&Constraints> {
        match self {
            Verdict::Rewrite(constraints) => Some(constraints),
            Verdict::Deny => None,
        }
    }
}

/// Decides one request against the policies of one endpoint (GW4, GW5, GW10).
///
/// `now` is passed in rather than read from the clock so a decision is reproducible: the
/// same inputs always yield the same verdict, which is what makes the dry-run endpoint
/// (GW13) honest.
pub fn evaluate(
    subject: &Subject,
    operation: Operation,
    request: &Request,
    tenant: &str,
    policies: &[PolicySpec],
    now: DateTime<Utc>,
) -> Verdict {
    let applicable: Vec<&PolicySpec> = policies
        .iter()
        .filter(|policy| applies(policy, subject, operation, now))
        .collect();

    // GW4: any matching prohibition ends the evaluation, whatever the permissions say.
    if applicable
        .iter()
        .any(|policy| policy.effect.is_prohibition() && covers_request(policy, request))
    {
        return Verdict::Deny;
    }

    let grants: Vec<&PolicySpec> = applicable
        .into_iter()
        .filter(|policy| !policy.effect.is_prohibition())
        .collect();

    // GW5: nothing granted the operation, so it is refused. This is also the answer for an
    // anonymous caller on an endpoint with no public grant.
    if grants.is_empty() {
        return Verdict::Deny;
    }

    Verdict::Rewrite(Box::new(intersect(
        operation, request, tenant, &grants, now,
    )))
}

/// Whether a policy speaks about this caller and this operation at all (GW7).
fn applies(
    policy: &PolicySpec,
    subject: &Subject,
    operation: Operation,
    now: DateTime<Utc>,
) -> bool {
    subject.is(&policy.assignee) && grants_operation(policy, operation) && in_force(policy, now)
}

fn grants_operation(policy: &PolicySpec, operation: Operation) -> bool {
    policy.operations.iter().any(|granted| match granted {
        OperationRef::Single(single) => *single == operation,
        OperationRef::Group(group) => group.operations().contains(&operation),
    })
}

/// Whether the policy is inside its validity window (GW7).
fn in_force(policy: &PolicySpec, now: DateTime<Utc>) -> bool {
    policy.validity.as_ref().is_none_or(|validity| {
        validity.from.is_none_or(|from| now >= from) && validity.to.is_none_or(|to| now < to)
    })
}

/// Whether a prohibition speaks about what this request touches.
///
/// A prohibition with no entity selector covers the whole operation; one that names types
/// covers the request when the caller asked for a type it names, or asked for no type at
/// all, in which case the caller's result set would include the prohibited type.
fn covers_request(policy: &PolicySpec, request: &Request) -> bool {
    let prohibited = granted_types(&policy.information);
    prohibited.is_empty() || request.types.is_empty() || !prohibited.is_disjoint(&request.types)
}

/// Intersects the request with the union of the grants (GW10, GW11).
fn intersect(
    operation: Operation,
    request: &Request,
    tenant: &str,
    grants: &[&PolicySpec],
    now: DateTime<Utc>,
) -> Constraints {
    let granted_types: BTreeSet<String> = grants
        .iter()
        .flat_map(|policy| granted_types(&policy.information))
        .collect();
    // A grant that names no attribute grants the whole entity, so beside it no other
    // grant's list may narrow the answer: a steward who may also set `status` still reads
    // everything the public reads (EP-16). The union of the lists holds only when every
    // grant has one.
    let granted_attrs: BTreeSet<String> = if grants
        .iter()
        .any(|policy| granted_attrs(&policy.information).is_empty())
    {
        BTreeSet::new()
    } else {
        grants
            .iter()
            .flat_map(|policy| granted_attrs(&policy.information))
            .collect()
    };

    let types = narrow(&request.types, &granted_types);
    // A write carries no attribute selection: its attributes are in the body, which the
    // write guard checks against the grant's own set. Narrowing by a query parameter
    // here would let `?attrs=x` empty that set, and an empty set is "no projection"
    // (T-0805, GW11, GW17).
    let attrs = if operation.is_write() {
        granted_attrs.clone()
    } else {
        narrow(&request.attrs, &granted_attrs)
    };

    // The caller named types, the grants name types, and nothing is left: the answer is
    // genuinely nothing. It has to be said here, because an empty `types` set means "no
    // type filter" downstream, and forwarding no filter would ask the broker for every
    // type in the tenant instead of none of them (T-0381, GW10, R20).
    let no_type_left = !request.types.is_empty() && !granted_types.is_empty() && types.is_empty();
    // The same for attributes: the caller named some, the grants name some, none is shared.
    // An empty `attrs` downstream is "no projection", the opposite of what was decided.
    let no_attr_left = !request.attrs.is_empty() && !granted_attrs.is_empty() && attrs.is_empty();

    let filters: Vec<String> = grants
        .iter()
        .filter_map(|policy| policy_filter(policy))
        .collect();
    let geo = geo::intersect(
        request.geo_q.as_deref(),
        &grants
            .iter()
            .filter_map(|policy| policy.geo_q.as_deref())
            .collect::<Vec<_>>(),
    );
    let clamped = temporal::clamp(
        request.temporal_q.as_deref(),
        &grants
            .iter()
            .filter_map(|policy| policy.temporal_q.as_deref())
            .collect::<Vec<_>>(),
        now,
    );

    Constraints {
        tenant: tenant.to_owned(),
        // The signal describes the answer, not the request (R22, AG-13, T-1213). A read that
        // names no `attrs` is the ordinary read, and it is the one that most needs telling:
        // its entities come back with whatever the grant's whitelist left out and no other
        // way to know why. Comparing against `request.attrs` said nothing there, because a
        // request that asked for nothing cannot have had anything taken from it.
        restricted: types.len() < request.types.len()
            || attrs.len() < request.attrs.len()
            || !granted_attrs.is_empty()
            || !filters.is_empty()
            || geo.restricted
            || clamped.restricted,
        types,
        // An exact `id` is the pattern that matches only it; until T-0806 it was never
        // compiled, so a selector naming one entity granted the whole type.
        id_patterns: grants
            .iter()
            .flat_map(|policy| policy.information.iter())
            .flat_map(|info| info.entities.iter())
            .flat_map(|selector| {
                selector.id_pattern.clone().into_iter().chain(
                    selector
                        .id
                        .as_ref()
                        .map(|urn| format!("^{}$", regex::escape(&urn.to_string()))),
                )
            })
            .collect(),
        attrs,
        // The grants alone say which attributes this caller may read, not which type each one
        // belongs to; a projection fills this in (MP-02, T-1862).
        attrs_by_type: BTreeMap::new(),
        served: granted_attrs.clone(),
        hidden: BTreeSet::new(),
        q: conjoin(request.q.as_deref(), &filters),
        granted_scopes: union(grants.iter().filter_map(|policy| policy.scope_q.as_deref())),
        geo_q: geo.geo_q,
        geo_grants: geo.grants,
        geo_areas: geo.areas,
        geo_caller: geo.caller,
        temporal_q: clamped.temporal_q,
        temporal_windows: clamped.windows,
        empty: clamped.empty || no_type_left || no_attr_left,
        // Filled by the filter rule, which runs after this and knows which types it took out.
        dropped: BTreeSet::new(),
    }
}

/// One policy's whole residual as one `q` conjunction (R12, R13).
///
/// This is what stops the cross-policy bleed of ADR 006: a policy's `q` and its scopes
/// travel together in one parenthesized term, so no expression can ever pair one policy's
/// filter with another policy's area.
fn policy_filter(policy: &PolicySpec) -> Option<String> {
    let parts: Vec<String> = [
        policy
            .q
            .as_deref()
            .filter(|filter| is_balanced(filter))
            .map(|filter| format!("({filter})")),
        policy.scope_q.as_deref().and_then(scope_folding::fold),
    ]
    .into_iter()
    .flatten()
    .collect();

    match parts.len() {
        0 => None,
        1 => parts.into_iter().next(),
        _ => Some(format!("({})", parts.join(";"))),
    }
}

/// The scope tree the grants cover, for the write guard alone (R29).
fn union<'a>(scopes: impl Iterator<Item = &'a str>) -> Option<String> {
    let scopes: Vec<&str> = scopes.collect();
    (!scopes.is_empty()).then(|| scopes.join("|"))
}

/// `requested ∩ granted`, or the granted set when the caller asked for nothing specific
/// (GW11). Never the requested set: that would let a caller name what it was not given.
pub fn narrow(requested: &BTreeSet<String>, granted: &BTreeSet<String>) -> BTreeSet<String> {
    if granted.is_empty() {
        return requested.clone();
    }
    if requested.is_empty() {
        return granted.clone();
    }
    requested.intersection(granted).cloned().collect()
}

/// `narrow`, except that two whitelists sharing nothing leave the entity's identity (`id`,
/// `type`) rather than an empty set, which downstream would read as "no projection"
/// (T-0812, R9).
pub fn narrow_to_identity(
    requested: &BTreeSet<String>,
    granted: &BTreeSet<String>,
) -> BTreeSet<String> {
    let narrowed = narrow(requested, granted);
    if narrowed.is_empty() && !requested.is_empty() && !granted.is_empty() {
        return ["id", "type"].into_iter().map(str::to_owned).collect();
    }
    narrowed
}

/// The policies that name this caller and are in force now, whatever they grant (EP-55).
///
/// The access surface reports what a caller may do, which is a question about the policy
/// set rather than about one request, so it starts here rather than at [`evaluate`].
pub fn effective<'a>(
    subject: &Subject,
    policies: &'a [PolicySpec],
    now: DateTime<Utc>,
) -> Vec<&'a PolicySpec> {
    policies
        .iter()
        .filter(|policy| subject.is(&policy.assignee) && in_force(policy, now))
        .collect()
}

/// Every operation a policy grants, by its CIM 009 name, with the Table 4.20-2 groups
/// expanded (R8).
pub fn granted_operations(policy: &PolicySpec) -> BTreeSet<&'static str> {
    policy
        .operations
        .iter()
        .flat_map(|granted| match granted {
            OperationRef::Single(single) => vec![single.as_str()],
            OperationRef::Group(group) => {
                group.operations().iter().map(Operation::as_str).collect()
            }
        })
        .collect()
}

/// The entity types a policy's registration information names.
pub fn granted_types(information: &[RegistrationInfo]) -> BTreeSet<String> {
    information
        .iter()
        .flat_map(|info| info.entities.iter())
        .map(|selector| selector.entity_type.clone())
        .collect()
}

/// The properties and relationships a policy's registration information names (R9).
pub fn granted_attrs(information: &[RegistrationInfo]) -> BTreeSet<String> {
    information
        .iter()
        .flat_map(|info| {
            info.property_names
                .iter()
                .chain(info.relationship_names.iter())
        })
        .cloned()
        .collect()
}

/// The caller's filter AND the union of the per-policy filters, in NGSI-LD's own syntax
/// (`;` is AND, `|` is OR, and parentheses group).
///
/// Every operand is parenthesized, so no operand can reach outside itself and change how
/// its neighbours are grouped. That only holds if the caller's filter is balanced, which
/// `is_balanced` establishes: an unbalanced filter is dropped rather than conjoined,
/// leaving the grants alone in force.
pub fn conjoin(requested: Option<&str>, filters: &[String]) -> Option<String> {
    let union = match filters.len() {
        0 => None,
        1 => Some(format!("({})", filters[0])),
        _ => Some(format!(
            "({})",
            filters
                .iter()
                .map(|filter| format!("({filter})"))
                .collect::<Vec<_>>()
                .join("|")
        )),
    };

    match (requested.filter(|filter| is_balanced(filter)), union) {
        (Some(caller), Some(grants)) => Some(format!("({caller});{grants}")),
        (Some(caller), None) => Some(format!("({caller})")),
        (None, grants) => grants,
    }
}

/// Whether a filter's parentheses are balanced, ignoring those inside a quoted value.
///
/// A caller's filter is wrapped in parentheses before it is conjoined with the grants. An
/// unbalanced filter would consume that wrapping and change the grouping of what follows,
/// which is exactly the cross-policy bleed ADR 006 is about, so it never gets wrapped.
fn is_balanced(filter: &str) -> bool {
    let mut depth: i32 = 0;
    let mut quoted = false;
    let mut escaped = false;

    for character in filter.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        match character {
            '\\' if quoted => escaped = true,
            '"' | '\'' => quoted = !quoted,
            '(' if !quoted => depth += 1,
            ')' if !quoted => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            }
            _ => {}
        }
    }
    depth == 0 && !quoted
}
