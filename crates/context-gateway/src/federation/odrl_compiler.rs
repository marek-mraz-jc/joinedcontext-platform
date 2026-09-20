//! An accepted ODRL 2.2 agreement, compiled into the `Policy` entities that enforce it
//! (T-0179, DS-10, DS-03, R26, R52, MIM3-R10).
//!
//! Negotiation ends in a document; enforcement reads manifests. This module is the one step
//! between them, and it is deliberately the only one: a consumer's access is whatever the
//! compiled `Policy` says, so an agreement that was never compiled grants nothing at all, and
//! an agreement compiled twice produces the same policies.
//!
//! Reading the document is not re-implemented here.
//! [`crate::handlers::access_odrl::read`] already takes an ODRL document apart into the six
//! R26 names, because the access surface has to round-trip its own output; a second parser
//! would be a second opinion about what an agreement says. The path is spelled out because
//! the `use` below imports the module as `{self, Grant}`, and rustdoc does not resolve a
//! link through a `self` import.
//!
//! **The ceiling (DS-03).** A negotiated agreement is a subset of an offer, and nothing in the
//! document itself proves that. So every compiled policy is checked against the offered
//! endpoint's own policies and the compilation fails if it would reach further:
//!
//! | dimension | rule |
//! |---|---|
//! | operations, entity types | must be offered; anything else is refused |
//! | attributes | the offer's whitelist wins; an agreement naming none inherits it |
//! | `q`, `scopeQ` | conjunction of both, because a filter the agreement did not repeat is still in force |
//! | `geoQ`, `temporalQ` | the offer's when the agreement names none; a different one is refused |
//!
//! The last row is where this compiler declines to be clever. Deciding that one polygon lies
//! inside another, or one interval inside another, is geometry and calendar arithmetic; a
//! wrong answer silently widens a fence somebody drew on a map. Refusing is the only answer
//! that cannot be wrong.

use crate::handlers::access_odrl::{self, Grant};
use crate::pdp::evaluator::{granted_attrs, granted_operations};
use jc_core::envelope::Ref;
use jc_core::kinds::{
    AgreementConstraints, DataAgreementSpec, EntitySelector, OperationRef, PolicyEffect,
    PolicySpec, Principal, PrincipalKind, RegistrationInfo, Validity,
};
use jc_core::Urn;
use serde_json::Value;
use std::collections::BTreeSet;
use std::str::FromStr;

/// What one accepted agreement becomes.
#[derive(Debug, Clone, PartialEq)]
pub struct Compiled {
    /// The policies to write into the space, one per rule of the agreement.
    pub policies: Vec<PolicySpec>,
    /// The residual filters that ended up in force, for the `DataAgreement` to record (DS-10).
    pub constraints: AgreementConstraints,
}

/// Why an agreement could not be compiled.
///
/// Every variant names the dimension and the value, because the operator who reads this is
/// deciding whether the connector negotiated something it should not have.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CompileError {
    /// The document is not an ODRL `Agreement`.
    #[error("the document is a {0:?}, and only an Agreement is compiled into policies (DS-10)")]
    NotAnAgreement(String),
    /// The agreement names no consumer, or one that is not a DID.
    #[error("the agreement's assignee is {0:?}, and a consumer is identified by a did: (DS-04)")]
    NotADid(Option<String>),
    /// The agreement names a consumer other than the one the negotiation recorded.
    #[error("the agreement is assigned to {found}, but the negotiation recorded {expected}")]
    WrongConsumer {
        /// The DID in the document.
        found: String,
        /// The DID in the `DataAgreement`.
        expected: String,
    },
    /// The agreement carries no permission at all.
    #[error("the agreement grants nothing; an agreement with no permission is not an agreement")]
    GrantsNothing,
    /// A verb the closed CIM 009 vocabulary does not have (R8).
    #[error("{0} is not a CIM 009 operation, and the vocabulary is closed (R8)")]
    UnknownOperation(String),
    /// The agreement reaches past what the endpoint offered (DS-03).
    #[error("the agreement asks for {dimension} {asked}, which the offer does not grant (DS-03)")]
    Widens {
        /// Which of the four dimensions overreached.
        dimension: &'static str,
        /// The value that is not covered.
        asked: String,
    },
    /// A target the agreement names in a form a `Policy` cannot express.
    #[error("{0}")]
    Unrepresentable(String),
}

/// Compiles one accepted agreement against the policies of the endpoint that was offered.
///
/// `space` is the context space the compiled policies belong to, `offered` the endpoint's own
/// policy set, which is the ceiling. Prohibitions in the agreement are compiled too and are
/// never checked against the ceiling: a rule that takes access away cannot widen one.
pub fn compile(
    agreement: &Value,
    negotiated: &DataAgreementSpec,
    offered: &[PolicySpec],
    space: &str,
) -> Result<Compiled, CompileError> {
    let kind = agreement
        .get("@type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if kind != "Agreement" {
        return Err(CompileError::NotAnAgreement(kind.to_owned()));
    }

    let consumer = agreement
        .get("assignee")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let consumer = match consumer {
        Some(did) if did.starts_with("did:") => did,
        other => return Err(CompileError::NotADid(other)),
    };
    if consumer != negotiated.remote_participant.as_str() {
        return Err(CompileError::WrongConsumer {
            found: consumer,
            expected: negotiated.remote_participant.as_str().to_owned(),
        });
    }

    let grants = access_odrl::read(agreement);
    if !grants.iter().any(|grant| !grant.prohibition) {
        return Err(CompileError::GrantsNothing);
    }

    let fallback_assigner = agreement.get("assigner").and_then(Value::as_str);
    let mut policies = Vec::new();
    for grant in &grants {
        policies.push(policy_of(
            grant,
            &consumer,
            fallback_assigner,
            negotiated,
            offered,
            space,
        )?);
    }

    Ok(Compiled {
        constraints: constraints_of(&policies),
        policies,
    })
}

/// One ODRL rule as one `Policy`, already narrowed to what the offer allows.
fn policy_of(
    grant: &Grant,
    consumer: &str,
    fallback_assigner: Option<&str>,
    negotiated: &DataAgreementSpec,
    offered: &[PolicySpec],
    space: &str,
) -> Result<PolicySpec, CompileError> {
    let assigner = grant
        .assigner
        .as_deref()
        .or(fallback_assigner)
        .ok_or_else(|| {
            CompileError::Unrepresentable(
                "the agreement names no assigner, and a Policy has to say who granted it"
                    .to_owned(),
            )
        })?;

    let operations = operations_of(grant)?;
    // A prohibition subtracts; it can only ever narrow, so the ceiling does not apply to it.
    let ceiling = if grant.prohibition {
        None
    } else {
        Some(ceiling_for(grant, &operations, offered)?)
    };

    let selector = EntitySelector {
        entity_type: grant.entity_type.clone(),
        id: grant
            .id
            .as_deref()
            .map(Urn::from_str)
            .transpose()
            .map_err(|error| CompileError::Unrepresentable(error.to_string()))?,
        id_pattern: grant.id_pattern.clone(),
    };

    let attributes = match &ceiling {
        // An agreement that names no attribute is asking for what the offer gives, which is
        // the offer's own whitelist and not "everything".
        Some(ceiling) if grant.attributes.is_empty() => {
            ceiling.attributes.iter().cloned().collect()
        }
        Some(ceiling) if !ceiling.attributes.is_empty() => {
            if let Some(extra) = grant
                .attributes
                .iter()
                .find(|name| !ceiling.attributes.contains(*name))
            {
                return Err(CompileError::Widens {
                    dimension: "the attribute",
                    asked: extra.clone(),
                });
            }
            grant.attributes.clone()
        }
        _ => grant.attributes.clone(),
    };

    let (q, scope_q, geo_q, temporal_q) = residuals(grant, ceiling.as_ref())?;

    Ok(PolicySpec {
        context_space_ref: Ref::Name(space.to_owned()),
        effect: if grant.prohibition {
            PolicyEffect::Prohibition
        } else {
            PolicyEffect::Permission
        },
        assigner: assigner.to_owned(),
        // DS-10: the consumer's DID is the assignee, so the PDP matches the token of the
        // participant the agreement was signed with and nobody else.
        assignee: Principal::new(PrincipalKind::Did, consumer),
        operations,
        information: vec![RegistrationInfo {
            entities: vec![selector],
            property_names: attributes,
            relationship_names: Vec::new(),
        }],
        q,
        scope_q,
        geo_q,
        temporal_q,
        // DS-10, DS-12: the policy expires with the agreement, so an expiry needs no second
        // action to take effect.
        validity: Some(Validity {
            from: negotiated.validity.from,
            to: negotiated.validity.to,
        }),
    })
}

/// The rule's operations, through the closed CIM 009 vocabulary (R8).
///
/// Parsed with serde rather than a match of our own: `OperationRef` is untagged over the two
/// enums, so a verb neither of them has is rejected here and cannot reach a manifest.
fn operations_of(grant: &Grant) -> Result<Vec<OperationRef>, CompileError> {
    grant
        .operations
        .iter()
        .map(|name| {
            serde_json::from_value(Value::String(name.clone()))
                .map_err(|_| CompileError::UnknownOperation(name.clone()))
        })
        .collect()
}

/// What the offer allows for one rule's entity type.
#[derive(Debug, Clone, Default)]
struct Ceiling {
    /// The attribute whitelist of the offering policy; empty means every attribute.
    attributes: BTreeSet<String>,
    /// The residual filters the offer already imposes.
    q: Option<String>,
    scope_q: Option<String>,
    geo_q: Option<String>,
    temporal_q: Option<String>,
}

/// The offer's ceiling for one rule, refusing the rule if the offer does not cover it (DS-03).
fn ceiling_for(
    grant: &Grant,
    operations: &[OperationRef],
    offered: &[PolicySpec],
) -> Result<Ceiling, CompileError> {
    let covering: Vec<&PolicySpec> = offered
        .iter()
        .filter(|policy| !policy.effect.is_prohibition())
        .filter(|policy| targets(policy, &grant.entity_type))
        .collect();
    if covering.is_empty() {
        return Err(CompileError::Widens {
            dimension: "the entity type",
            asked: grant.entity_type.clone(),
        });
    }

    let permitted: BTreeSet<&str> = covering
        .iter()
        .flat_map(|policy| granted_operations(policy))
        .collect();
    for operation in operations {
        if !permitted.contains(operation.as_str()) {
            return Err(CompileError::Widens {
                dimension: "the operation",
                asked: operation.as_str().to_owned(),
            });
        }
    }

    // The most permissive covering policy is the ceiling: the consumer could have negotiated
    // against that one, so anything inside it is inside the offer. "Most permissive" is a
    // whitelist of every attribute first, then the longest whitelist, then the fewest filters.
    let widest = covering
        .iter()
        .max_by_key(|policy| {
            let attrs = granted_attrs(&policy.information);
            (
                usize::from(attrs.is_empty()),
                attrs.len(),
                usize::from(policy.q.is_none()),
            )
        })
        .expect("covering is not empty");

    Ok(Ceiling {
        attributes: granted_attrs(&widest.information),
        q: widest.q.clone(),
        scope_q: widest.scope_q.clone(),
        geo_q: widest.geo_q.clone(),
        temporal_q: widest.temporal_q.clone(),
    })
}

/// Whether one offered policy targets this entity type.
fn targets(policy: &PolicySpec, entity_type: &str) -> bool {
    policy
        .information
        .iter()
        .flat_map(|info| info.entities.iter())
        .any(|selector| selector.entity_type == entity_type)
}

/// The four residual filters of one compiled policy.
type Residuals = (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
);

/// What ends up in force, against the ceiling's own filters (DS-03, DS-10).
fn residuals(grant: &Grant, ceiling: Option<&Ceiling>) -> Result<Residuals, CompileError> {
    let Some(ceiling) = ceiling else {
        return Ok((
            grant.q.clone(),
            grant.scope_q.clone(),
            grant.geo_q.clone(),
            grant.temporal_q.clone(),
        ));
    };
    Ok((
        conjunction(grant.q.as_deref(), ceiling.q.as_deref()),
        conjunction(grant.scope_q.as_deref(), ceiling.scope_q.as_deref()),
        indivisible("geoQ", grant.geo_q.as_deref(), ceiling.geo_q.as_deref())?,
        indivisible(
            "temporalQ",
            grant.temporal_q.as_deref(),
            ceiling.temporal_q.as_deref(),
        )?,
    ))
}

/// Two NGSI-LD filters that both have to hold, which `q` and `scopeQ` write with `;`.
///
/// A filter the agreement did not repeat is still one the offer imposed, so the conjunction is
/// what keeps an unmentioned constraint in force rather than dropping it (DS-03).
fn conjunction(agreed: Option<&str>, offered: Option<&str>) -> Option<String> {
    match (agreed, offered) {
        (None, offered) => offered.map(str::to_owned),
        (Some(agreed), None) => Some(agreed.to_owned()),
        (Some(agreed), Some(offered)) if agreed == offered => Some(agreed.to_owned()),
        (Some(agreed), Some(offered)) => Some(format!("({agreed});({offered})")),
    }
}

/// A filter that cannot be conjoined: one geometry or one interval, not two.
///
/// Where the two differ this refuses rather than picks. Whether one polygon lies inside
/// another is a geometry question, and a compiler that guesses wrong silently widens a fence
/// somebody drew on a map; the negotiation is the place to settle it, not this function.
fn indivisible(
    dimension: &'static str,
    agreed: Option<&str>,
    offered: Option<&str>,
) -> Result<Option<String>, CompileError> {
    match (agreed, offered) {
        (None, offered) => Ok(offered.map(str::to_owned)),
        (Some(agreed), None) => Ok(Some(agreed.to_owned())),
        (Some(agreed), Some(offered)) if agreed == offered => Ok(Some(agreed.to_owned())),
        (Some(agreed), Some(_)) => Err(CompileError::Widens {
            dimension: if dimension == "geoQ" {
                "the geographic constraint"
            } else {
                "the temporal constraint"
            },
            asked: agreed.to_owned(),
        }),
    }
}

/// The residuals of the compiled permissions, as the `DataAgreement` records them (DS-10).
///
/// Only the permissions: a prohibition's filter selects what is taken away, and recording it
/// beside the grants would read as a narrowing of them.
fn constraints_of(policies: &[PolicySpec]) -> AgreementConstraints {
    let permissions: Vec<&PolicySpec> = policies
        .iter()
        .filter(|policy| !policy.effect.is_prohibition())
        .collect();
    let one = |pick: fn(&PolicySpec) -> Option<&String>| -> Option<String> {
        let distinct: BTreeSet<&String> = permissions.iter().filter_map(|p| pick(p)).collect();
        match distinct.len() {
            0 => None,
            1 => distinct.into_iter().next().cloned(),
            // Several rules with different residuals: the agreement's constraint is the
            // disjunction, because a request satisfying any one rule is permitted by it.
            _ => Some(
                distinct
                    .into_iter()
                    .map(|filter| format!("({filter})"))
                    .collect::<Vec<_>>()
                    .join("|"),
            ),
        }
    };
    AgreementConstraints {
        q: one(|policy| policy.q.as_ref()),
        scope_q: one(|policy| policy.scope_q.as_ref()),
        geo_q: one(|policy| policy.geo_q.as_ref()),
        temporal_q: one(|policy| policy.temporal_q.as_ref()),
    }
}
