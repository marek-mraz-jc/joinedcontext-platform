//! Read, evaluate, then write conditionally (T-0152, R45, GW16).
//!
//! GW16 tests the payload a caller sent. That is the whole answer for a grant partitioned
//! by URN prefix, because an id never changes. It is only half of it for a grant that
//! carries a `q` or a polygon: the payload can sit inside the grant while the entity as it
//! is *stored* sits outside it, and between the check and the write the entity can move.
//!
//! So a write on such a grant is run the way RFC 9110 runs a conditional request. The
//! gateway reads the entity, evaluates the grant against what it read, and forwards the
//! write with `If-Match` set to the entity tag it read, which turns a lost race into a
//! `412` the caller can retry. The caller's own `If-Match` is checked by the same read.
//!
//! The `q` half is not evaluated here. The gateway sends the grant's own filter to the
//! broker and keeps what comes back, because a second implementation of the query language
//! is a second implementation that can disagree with the one holding the data.

use crate::pdp::evaluator::Constraints;
use crate::pdp::{geo, projection};
use crate::proxy::Broker;
use crate::{operations, query};
use axum::body::Body;
use axum::http::header::{ACCEPT, ETAG, IF_MATCH};
use axum::http::{HeaderMap, HeaderValue, Method};
use jc_core::kinds::Operation;
use jc_core::ProblemDetails;
use serde_json::Value;

/// The tenant the gateway pinned, which the reads before a write carry too (GW20).
const TENANT: &str = "NGSILD-Tenant";

/// The largest answer a pre-write read will hold. One entity, generously.
const MAX_PROBE: usize = 1024 * 1024;

/// Whether the grants decide from the entity as it is stored, rather than from the payload
/// the caller sent alone (R45).
pub fn state_dependent(constraints: &Constraints) -> bool {
    constraints.q.is_some() || !constraints.geo_areas.is_empty()
}

/// Whether this write has to be preceded by a read.
///
/// Only writes that address one entity: a create has no stored state to read, and a batch
/// addresses its entities in the payload rather than in the path.
pub fn required(
    operation: Operation,
    path: &str,
    headers: &HeaderMap,
    constraints: &Constraints,
) -> bool {
    operation.is_write()
        && operations::addressed_entity(path).is_some()
        && (headers.contains_key(IF_MATCH) || state_dependent(constraints))
}

/// The batch write a state-dependent grant cannot decide (R45, GW16, GW18, T-0807).
///
/// A batch addresses its entities in the payload, so there is no one entity to read and no
/// `If-Match` to carry: the payload would be the only thing consulted, and a payload can sit
/// inside the grant while the entity it names is stored outside it. `entityOperations/create`
/// is not one of these — it creates, and a create has no stored state — so what is refused is
/// upsert, update, merge and delete. R45 answers this case by partitioning write authority by
/// URN prefix rather than by a condition, which is what the detail points the caller at.
pub fn batch_refusal(operation: Operation, constraints: &Constraints) -> Option<ProblemDetails> {
    let touches_stored_entities = matches!(
        operation,
        Operation::UpsertBatch
            | Operation::UpdateBatch
            | Operation::MergeBatch
            | Operation::DeleteBatch
    );
    (touches_stored_entities && state_dependent(constraints)).then(|| {
        ProblemDetails::forbidden().with_detail(
            "this caller's write authority is decided from the stored entity, which a batch \
             operation does not address; write one entity per request",
        )
    })
}

/// What the read before the write decided.
#[derive(Debug)]
pub enum Precondition {
    /// Forward the write, carrying this `If-Match` when the broker published an entity tag.
    Forward(Option<HeaderValue>),
    /// Answer this and write nothing.
    Refuse(ProblemDetails),
}

/// The `412` a failed precondition answers with.
///
/// CIM 009 clause 5.5.3 has no error type for it, so this keeps the platform's own problem
/// type: a client that keys on `type` learns something true rather than an ETSI term that
/// means something else.
pub fn precondition_failed(detail: &str) -> ProblemDetails {
    ProblemDetails::new(412, "precondition-failed", "Precondition Failed").with_detail(detail)
}

/// Whether an `If-Match` header is satisfied by the entity tag the broker published
/// (RFC 9110 section 13.1.1).
///
/// `*` asks only that a current representation exist, which the caller of this function has
/// just read. Everything else is the strong comparison: a weak tag satisfies nothing, not
/// even itself, and no tag at all satisfies nothing either. A broker that publishes no
/// `ETag` therefore makes every explicit `If-Match` fail rather than pass unnoticed — a
/// precondition the platform cannot evaluate must not be treated as met.
pub fn matches(if_match: &str, etag: Option<&str>) -> bool {
    if if_match.trim() == "*" {
        return true;
    }
    let Some(etag) = etag.map(str::trim) else {
        return false;
    };
    if !etag.starts_with('"') {
        return false;
    }
    if_match
        .split(',')
        .map(str::trim)
        .any(|candidate| candidate == etag)
}

/// Reads the entity a write addresses and decides what may happen to it (R45, GW16).
pub async fn evaluate(
    broker: &Broker,
    path: &str,
    headers: &HeaderMap,
    constraints: &Constraints,
) -> Precondition {
    let Some(id) = operations::addressed_entity(path) else {
        return Precondition::Forward(None);
    };
    let if_match = headers
        .get(IF_MATCH)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);

    let read = match retrieve(broker, &format!("/ngsi-ld/v1/entities/{id}"), headers).await {
        Ok(read) => read,
        Err(problem) => return Precondition::Refuse(*problem),
    };
    if read.status == 404 {
        return match if_match {
            // RFC 9110 section 13.1.1: a precondition on a target with no current
            // representation fails, whatever the tag says.
            Some(_) => Precondition::Refuse(precondition_failed(
                "the entity this write addresses does not exist, so no entity tag matches it",
            )),
            None => Precondition::Refuse(ProblemDetails::not_found()),
        };
    }
    if !(200..300).contains(&read.status) {
        tracing::error!(
            status = read.status,
            "the read before a write did not answer"
        );
        return Precondition::Refuse(ProblemDetails::new(
            502,
            "upstream-unavailable",
            "Broker Unavailable",
        ));
    }

    // The stored entity has to be one this caller may see at all, by the two filters a read
    // is narrowed by here rather than at the broker (R24, GW11). A write to an entity the
    // caller cannot see is a miss, not a refusal (R20).
    // Every granted area, not only the ones a read filters here: this read is a retrieve by
    // id, which carries no `geoQ` at all, so nothing narrowed it upstream (T-0807).
    let areas = geo::Areas::of(&constraints.geo_areas, constraints.geo_caller.as_deref());
    if !projection::permitted(&read.body, constraints)
        || !areas.as_ref().is_none_or(|areas| areas.admits(&read.body))
    {
        return Precondition::Refuse(ProblemDetails::not_found());
    }

    if constraints.q.is_some() {
        match condition_holds(broker, id, headers, constraints).await {
            Err(problem) => return Precondition::Refuse(*problem),
            Ok(false) => return Precondition::Refuse(ProblemDetails::not_found()),
            Ok(true) => {}
        }
    }

    if let Some(sent) = &if_match {
        if !matches(sent, read.etag.as_deref()) {
            return Precondition::Refuse(precondition_failed(
                "the entity changed since the tag in If-Match was issued, or the broker \
                 publishes no entity tag for it",
            ));
        }
    }

    if read.etag.is_none() && state_dependent(constraints) {
        tracing::warn!(
            "the broker published no ETag, so this state-dependent write cannot be made \
             conditional and its check-to-write window stays open (R45)"
        );
    }
    Precondition::Forward(
        read.etag
            .as_deref()
            .and_then(|tag| HeaderValue::from_str(tag).ok()),
    )
}

/// Whether the grant's own filter still matches the stored entity, as the broker sees it.
async fn condition_holds(
    broker: &Broker,
    id: &str,
    headers: &HeaderMap,
    constraints: &Constraints,
) -> Result<bool, Box<ProblemDetails>> {
    // Everything the grant narrows except the parts a retrieve of the whole entity must not
    // carry: `attrs` would hide the `location` the geo check reads, and `temporalQ` is not a
    // parameter of this resource at all.
    let mut probe = constraints.clone();
    probe.types.clear();
    probe.attrs.clear();
    probe.temporal_q = None;
    probe.temporal_windows.clear();

    let narrowed = query::upstream(&[], &probe, &[]);
    let found = retrieve(
        broker,
        &format!("/ngsi-ld/v1/entities?id={id}&{narrowed}"),
        headers,
    )
    .await?;
    if !(200..300).contains(&found.status) {
        tracing::error!(
            status = found.status,
            "the broker refused the condition query before a write"
        );
        return Err(Box::new(ProblemDetails::new(
            502,
            "upstream-unavailable",
            "Broker Unavailable",
        )));
    }
    Ok(found
        .body
        .as_array()
        .is_some_and(|entities| !entities.is_empty()))
}

/// One answer to a read the gateway made on its own account.
struct Read {
    status: u16,
    etag: Option<String>,
    body: Value,
}

/// Reads from the broker with the pinned tenant and nothing else the caller sent.
///
/// A fresh header map rather than the request's: the write's `Content-Length`, its media
/// type and its own `If-Match` all describe a body this read does not send.
async fn retrieve(
    broker: &Broker,
    target: &str,
    headers: &HeaderMap,
) -> Result<Read, Box<ProblemDetails>> {
    let mut probe = HeaderMap::new();
    if let Some(tenant) = headers.get(TENANT) {
        probe.insert(TENANT, tenant.clone());
    }
    probe.insert(ACCEPT, HeaderValue::from_static("application/json"));

    let answer = broker
        .send(Method::GET, target, probe, Body::empty())
        .await
        .map_err(|error| Box::new(ProblemDetails::from(error)))?;
    let status = answer.status().as_u16();
    let etag = answer
        .headers()
        .get(ETAG)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let Ok(bytes) = axum::body::to_bytes(answer.into_body(), MAX_PROBE).await else {
        tracing::error!("the entity read before a write is larger than the gateway holds");
        return Err(Box::new(ProblemDetails::internal()));
    };
    Ok(Read {
        status,
        etag,
        body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    })
}
