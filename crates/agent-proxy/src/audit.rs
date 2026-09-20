//! Structured audit logging for proxied requests (AG-42).

pub struct AuditEntry<'a> {
    pub run_id: &'a str,
    pub user: &'a str,
    pub upstream: &'a str,
    pub method: &'a str,
    pub path: &'a str,
    pub status: u16,
    pub bytes: usize,
    pub duration_ms: u128,
}

pub fn log_request(entry: &AuditEntry) {
    tracing::info!(
        target: "agent_proxy::audit",
        run_id = %entry.run_id,
        user = %entry.user,
        upstream = %entry.upstream,
        method = %entry.method,
        path = %entry.path,
        status = entry.status,
        bytes = entry.bytes,
        duration_ms = entry.duration_ms,
        "proxied request"
    );
}
