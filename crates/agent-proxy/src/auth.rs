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

/// The longest run id this proxy will carry. A minted one is a 36-character UUID; the room above
/// that is for a longer identifier the Portal may mint one day, not for a payload.
const RUN_ID_MAX: usize = 64;

/// Whether a string is shaped like a run id: ASCII letters, digits and hyphens, and at most
/// [`RUN_ID_MAX`] of them.
///
/// The id arrives in a header the workspace writes and is spent in three places that all read a
/// name: the Portal lookup path, the `agent-run-<id>` mesh identity, and the audit line. A
/// separator, a percent sign or a space in it belongs to none of those, so it is refused at the
/// door rather than encoded at each use — the Portal is then never asked about a string that
/// cannot be a run, and no shape of one can be read as a path, a second header or a log line
/// (T-1695, AG-52).
pub fn run_id_is_well_formed(run_id: &str) -> bool {
    !run_id.is_empty()
        && run_id.len() <= RUN_ID_MAX
        && run_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

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

    // A run id that is not a name is refused before anybody is asked about it, and answered with
    // the same sentence as every other bad credential (T-1695).
    if !run_id_is_well_formed(run_id) {
        tracing::warn!(
            length = run_id.len(),
            "a presented run id is not shaped like a run id"
        );
        return Err(Box::new(
            jc_core::ProblemDetails::unauthorized().with_detail(REFUSED),
        ));
    }

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

#[cfg(test)]
mod tests {
    use super::run_id_is_well_formed;

    /// T-1695: a run id is a name, so nothing that is a path, a second header or a query gets in.
    #[test]
    fn only_a_name_is_a_run_id() {
        for id in ["e3b0c442-98fc-1c14-9afb-4c7b2756a120", "r", "AGENT-RUN-1"] {
            assert!(run_id_is_well_formed(id), "{id:?} is a run id");
        }
        for id in [
            "",
            "..",
            "../../admin",
            "..%2f..%2fadmin",
            "%2e%2e/admin",
            "a/b",
            "a\\b",
            "a b",
            "a?b=1",
            "a#b",
            "a:b",
            "a.b",
            "a_b",
            "héllo",
            &"a".repeat(65),
        ] {
            assert!(!run_id_is_well_formed(id), "{id:?} passed as a run id");
        }
        assert!(run_id_is_well_formed(&"a".repeat(64)), "64 is the ceiling");
    }
}
