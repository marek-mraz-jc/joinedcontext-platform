//! Run ticket authentication and service mesh identity verification.

use crate::config::Config;
use crate::runs::{RunContext, RunResolver};
use argon2::{Argon2, PasswordHash, PasswordVerifier};
use axum::http::HeaderMap;
use std::sync::Arc;

pub const RUN_HEADER: &str = "x-jc-run";
pub const TICKET_HEADER: &str = "x-jc-ticket";
/// The bearer form of the same credential, for a client that cannot set headers of its own.
///
/// An OpenAI-compatible model client sends one thing and one thing only: `Authorization: Bearer
/// <api key>`. The workspace has no key, it has a ticket, so the ticket travels in that slot as
/// `jcr_<run id>.<ticket>` and is verified exactly as the two headers are (AG-52, ADR-N-020).
pub const TICKET_BEARER_PREFIX: &str = "jcr_";

/// The one sentence every refused credential is answered with, so no pair of answers tells a caller
/// which run ids exist (T-2285, EP-26, R20).
const REFUSED: &str = "invalid run credentials";

/// The run and ticket a request presents, in either of the two forms.
fn credentials(headers: &HeaderMap) -> Option<(String, String)> {
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    };
    if let (Some(run), Some(ticket)) = (header(RUN_HEADER), header(TICKET_HEADER)) {
        return Some((run, ticket));
    }
    let bearer = header("authorization")?;
    let token = bearer.strip_prefix("Bearer ")?.trim();
    let (run, ticket) = token.strip_prefix(TICKET_BEARER_PREFIX)?.split_once('.')?;
    if run.is_empty() || ticket.is_empty() {
        return None;
    }
    Some((run.to_owned(), ticket.to_owned()))
}

pub async fn authenticate(
    headers: &HeaderMap,
    resolver: &RunResolver,
    config: &Config,
) -> Result<Arc<RunContext>, Box<jc_core::ProblemDetails>> {
    let (run_id, ticket) = credentials(headers).ok_or_else(|| {
        Box::new(jc_core::ProblemDetails::unauthorized().with_detail(
            "missing run credentials: X-JC-Run with X-JC-Ticket, or Authorization: Bearer jcr_<run>.<ticket>",
        ))
    })?;
    let (run_id, ticket) = (run_id.as_str(), ticket.as_str());

    // One sentence for every way a presented credential can be wrong. An id the resolver does not
    // know and a ticket that does not verify used to answer differently ("invalid or inactive run"
    // against "invalid run credentials"), which made this route an oracle for run ids: a caller with
    // no ticket at all could read from the wording which ids are live, and a run id is a workspace's
    // branch name and its mesh identity (T-2285, EP-26, R20). Which of the two it was belongs in the
    // log, where an operator reads it and a caller cannot.
    let run = resolver.resolve(run_id).await.map_err(|_| {
        tracing::warn!(run = %run_id, "no active run holds this id");
        Box::new(jc_core::ProblemDetails::unauthorized().with_detail(REFUSED))
    })?;

    let parsed_hash = PasswordHash::new(&run.ticket_hash).map_err(|_| {
        // The stored hash is unusable, which is this platform's fault and not the caller's — but it
        // is not the caller's business either, so they are told what everybody else is told.
        tracing::error!(run = %run_id, "the stored ticket hash is not a PHC string");
        Box::new(jc_core::ProblemDetails::unauthorized().with_detail(REFUSED))
    })?;

    if Argon2::default()
        .verify_password(ticket.as_bytes(), &parsed_hash)
        .is_err()
    {
        tracing::warn!(run = %run_id, "the ticket presented for this run does not verify");
        return Err(Box::new(
            jc_core::ProblemDetails::unauthorized().with_detail(REFUSED),
        ));
    }

    if config.require_mesh_identity {
        let client_id = headers
            .get("l5d-client-id")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        if !is_runs_own_workload(client_id, &run.id) {
            return Err(Box::new(
                jc_core::ProblemDetails::forbidden().with_detail("mesh workload identity mismatch"),
            ));
        }
    }

    Ok(run)
}

/// Whether a Linkerd client identity is this run's own workload (AG-52).
///
/// A mesh identity is `<serviceaccount>.<namespace>.serviceaccount.identity.linkerd.cluster.local`
/// and the ServiceAccount is the workload, so the first label is compared whole. `contains` used to
/// admit any identity that merely held the string: a run whose id extends this one
/// (`agent-run-abc1…` for run `abc`) and any account or namespace named to hold it
/// (`x-agent-run-abc…`) passed as the second factor (T-1477).
fn is_runs_own_workload(client_id: &str, run_id: &str) -> bool {
    !run_id.is_empty()
        && client_id
            .split('.')
            .next()
            .is_some_and(|account| account == format!("agent-run-{run_id}"))
}
