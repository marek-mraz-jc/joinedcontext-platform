//! The reads the gateway makes before a write, on its own account (R45, GW16).
//!
//! The broker's view decides a write and compares its entity tag (T-3644, T-3789). What the
//! gateway still reads first are the entities a write's Relationships name, so it can refuse a
//! link to one the caller cannot see, and [`state_dependent`] says whether a grant decides from
//! a stored entity rather than from the payload alone.

use crate::pdp::evaluator::Constraints;
use crate::proxy::Broker;
use axum::body::Body;
use axum::http::header::ACCEPT;
use axum::http::{HeaderMap, HeaderValue, Method};
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

/// One answer to a read the gateway made on its own account.
pub(crate) struct Read {
    pub(crate) status: u16,
    pub(crate) body: Value,
}

/// Reads from the broker with the pinned tenant and nothing else the caller sent.
///
/// A fresh header map rather than the request's: the write's `Content-Length`, its media
/// type and its own `If-Match` all describe a body this read does not send.
pub(crate) async fn retrieve(
    broker: &Broker,
    target: &str,
    headers: &HeaderMap,
) -> Result<Read, Box<ProblemDetails>> {
    let mut probe = HeaderMap::new();
    if let Some(tenant) = headers.get(TENANT) {
        probe.insert(TENANT, tenant.clone());
    }
    probe.insert(ACCEPT, HeaderValue::from_static("application/json"));
    crate::middleware::tenancy::as_gateway(&mut probe);

    let answer = broker
        .send(Method::GET, target, probe, Body::empty())
        .await
        .map_err(|error| Box::new(ProblemDetails::from(error)))?;
    let status = answer.status().as_u16();
    let Ok(bytes) = axum::body::to_bytes(answer.into_body(), MAX_PROBE).await else {
        tracing::error!("the entity read before a write is larger than the gateway holds");
        return Err(Box::new(ProblemDetails::internal()));
    };
    Ok(Read {
        status,
        body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    })
}
