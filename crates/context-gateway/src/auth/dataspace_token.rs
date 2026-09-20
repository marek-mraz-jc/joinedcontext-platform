//! The transfer token an agreement issues, and the agreement behind it (T-0178, DS-01,
//! DS-02, DS-11, DS-12).
//!
//! A data space consumer reaches this platform the way everyone else does: through an
//! Endpoint, with a token whose audience is that Endpoint (RFC 8707). The token is an
//! ordinary realm token — the platform has one identity provider and adding a second trust
//! root for a connector would be exactly the credential DS-01 says a connector must not
//! hold — plus two claims the connector adds: `agreementId` and `participant`.
//!
//! The one thing that is easy to get wrong is whose token it is. The connector obtains it
//! with its own ServiceAccount, so `azp` names an account this repository declares and with
//! roles of its own. Resolving that account would hand a data space consumer the
//! connector's grants, which is the bypass DS-02 exists to prevent. So a token carrying
//! `agreementId` establishes the consumer's DID and nothing else, and every grant it gets
//! comes from the `Policy` entities the ODRL mapper compiled from the agreement.

use crate::auth::token::Claims;
use crate::pdp::evaluator::Subject;
use crate::resolver::Endpoint;
use chrono::{DateTime, Utc};
use jc_core::kinds::{AgreementRole, DataAgreementSpec};
use jcctl::loader::Repository;
use std::collections::BTreeMap;

/// The longest a transfer token may live, in seconds (DS-11).
pub const MAX_LIFETIME: i64 = 15 * 60;

/// One agreement, as the gateway needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Agreement {
    /// The project the agreement was negotiated in, which is the project an endpoint has to
    /// admit for a token under it to be usable there.
    pub project: String,
    /// The manifest, whose `state`, `validity` and `remoteParticipant` are what decide.
    pub spec: DataAgreementSpec,
}

/// Every agreement the repository declares, by its Dataspace Protocol identifier.
///
/// Replaced whole by the same reconcile that replaces the endpoint table, so a terminated
/// agreement stops being served at the same moment its compiled `Policy` entities go
/// (DS-12, OPS-45).
#[derive(Debug, Clone, Default)]
pub struct Agreements {
    by_id: BTreeMap<String, Agreement>,
}

impl Agreements {
    /// A gateway that knows of no agreement, which is what a repository without one
    /// describes and what every deployment had before the connector existed.
    pub fn new() -> Self {
        Self::default()
    }

    /// How many agreements are known.
    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    /// Whether the repository declares no agreement at all.
    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }

    /// The agreement this id names, only while it is one this platform serves data under.
    ///
    /// A consumer-role agreement is a token *this* platform holds to read somebody else's
    /// endpoint; it never authorises a read here, whatever its state.
    pub fn serving(&self, id: &str, now: DateTime<Utc>) -> Option<&Agreement> {
        self.by_id
            .get(id)
            .filter(|agreement| agreement.spec.role == AgreementRole::Provider)
            .filter(|agreement| agreement.spec.is_active(now))
    }
}

/// Why a transfer token establishes nobody.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Refused {
    /// The token names an agreement but no participant, or the other way round.
    #[error("a transfer token names both an agreement and a participant")]
    Incomplete,
    /// The token carries no issue time, so how long it was minted for cannot be measured.
    #[error("a transfer token says when it was issued")]
    NoLifetime,
    /// The token was minted to live longer than DS-11 allows.
    #[error("a transfer token lives at most {MAX_LIFETIME} seconds")]
    TooLong,
    /// No agreement of that id is being served: unknown, terminated, not yet in force, past
    /// its validity, or one where this platform is the consumer rather than the provider.
    #[error("agreement `{0}` is not one this platform serves data under")]
    NotServing(String),
    /// The token names a participant the agreement was not negotiated with.
    #[error("the transfer token names a participant agreement `{0}` does not")]
    WrongParticipant(String),
    /// The endpoint belongs to a project the agreement does not reach.
    #[error("agreement `{0}` does not reach this endpoint")]
    OutOfProject(String),
}

impl From<Refused> for jc_core::ProblemDetails {
    fn from(refused: Refused) -> Self {
        tracing::info!(%refused, "transfer token refused");
        match refused {
            // The token is not usable at all: the same 401 every other rejected credential
            // answers, so a probe learns nothing from which one it hit (R20).
            Refused::OutOfProject(_) => jc_core::ProblemDetails::forbidden(),
            _ => jc_core::ProblemDetails::unauthorized(),
        }
    }
}

/// Whether these claims are a transfer token at all.
///
/// One claim is enough to ask the question: a token carrying `agreementId` is claiming to
/// act under an agreement, and it is then checked as one rather than falling through to the
/// ServiceAccount that obtained it.
pub fn presented(claims: &Claims) -> bool {
    claims.agreement_id.is_some()
}

/// The caller a verified transfer token establishes (DS-02, DS-11, DS-12).
///
/// The signature, the issuer and the audience were checked before this: what is left is
/// everything about the agreement, and the lifetime the token was minted with.
pub fn subject(
    claims: &Claims,
    agreements: &Agreements,
    endpoint: &Endpoint,
    now: DateTime<Utc>,
) -> Result<Subject, Refused> {
    let (Some(id), Some(participant)) = (
        claims.agreement_id.as_deref().filter(|id| !id.is_empty()),
        claims.participant.as_deref().filter(|did| !did.is_empty()),
    ) else {
        return Err(Refused::Incomplete);
    };

    // DS-11 is a property of the token, not of the moment it is presented: a token minted
    // to live an hour is refused on its first second.
    let issued = claims.iat.ok_or(Refused::NoLifetime)?;
    if claims.exp.saturating_sub(issued) > MAX_LIFETIME {
        return Err(Refused::TooLong);
    }

    let agreement = agreements
        .serving(id, now)
        .ok_or_else(|| Refused::NotServing(id.to_owned()))?;
    if agreement.spec.remote_participant.as_str() != participant {
        return Err(Refused::WrongParticipant(id.to_owned()));
    }
    if !endpoint.admits(Some(&agreement.project)) {
        return Err(Refused::OutOfProject(id.to_owned()));
    }

    Ok(Subject {
        user: None,
        service_account: None,
        roles: std::collections::BTreeSet::new(),
        groups: std::collections::BTreeSet::new(),
        did: Some(participant.to_owned()),
        agreement: Some(id.to_owned()),
    })
}

/// The agreement table a loaded repository describes (CC-08, DS-12).
///
/// Deterministic like every other table the gateway builds from the repository, and just as
/// forgiving: an agreement the gateway cannot parse is left out rather than half-applied,
/// which means no token under it is honoured.
pub fn agreements_of(repo: &Repository) -> Agreements {
    let mut by_id: BTreeMap<String, Agreement> = BTreeMap::new();
    for (id, resource) in repo.iter() {
        if id.kind != "DataAgreement" {
            continue;
        }
        let Ok(spec) = serde_json::from_value::<DataAgreementSpec>(resource.manifest.spec.clone())
        else {
            continue;
        };
        if spec.validate().is_err() {
            continue;
        }
        by_id.insert(
            spec.agreement_id.clone(),
            Agreement {
                project: id.namespace.clone().unwrap_or_default(),
                spec,
            },
        );
    }
    Agreements { by_id }
}
