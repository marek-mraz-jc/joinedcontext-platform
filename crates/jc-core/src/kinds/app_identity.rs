//! The principal a `wasm` App's jobs run as (AP-159, T-3372, Architecture/12 §3).
//!
//! A job runs with no person behind it, so the App gets a ServiceAccount nobody writes,
//! `appjob-{name}`, bound to a Kubernetes ServiceAccount of its own, `appjob-{project}-{name}`,
//! whose federated Keycloak client `{project}-appjob-{name}` (PF-47) has one audience: the App's
//! own Endpoint. It holds no grant of its own; through that Endpoint it holds the caller role, so
//! the App's own Policies. `app-` was not free: `app-builder` is a hand-written account.

use crate::kinds::pipeline_identity::bounded;

/// The name prefix of a job principal: no ServiceAccount manifest may take it (AP-159).
pub const ACCOUNT_PREFIX: &str = "appjob-";

/// The job principal of the App `app`, `appjob-{app}`.
pub fn account_name(app: &str) -> String {
    bounded(&format!("{ACCOUNT_PREFIX}{app}"))
}

/// The Kubernetes ServiceAccount the principal is bound to, `appjob-{project}-{app}`: one per App
/// of the organization, since the identities namespace is shared.
pub fn kubernetes_service_account(project: &str, app: &str) -> String {
    bounded(&format!("{ACCOUNT_PREFIX}{project}-{app}"))
}

/// Whether a ServiceAccount name is one only an App's jobs may have (AP-159).
pub fn is_derived(name: &str) -> bool {
    name.starts_with(ACCOUNT_PREFIX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kinds::service_account::keycloak_client_id;

    #[test]
    fn an_apps_job_principal_its_kubernetes_account_and_client_are_its_own() {
        assert_eq!(account_name("kpi-forecast"), "appjob-kpi-forecast");
        assert_eq!(
            kubernetes_service_account("zilina", "kpi-forecast"),
            "appjob-zilina-kpi-forecast"
        );
        assert_eq!(
            keycloak_client_id("zilina", &account_name("kpi-forecast")),
            "zilina-appjob-kpi-forecast"
        );
        assert!(is_derived("appjob-x"));
        assert!(!is_derived("app-builder"));
    }

    #[test]
    fn a_long_name_stays_one_dns_label_and_stable() {
        let app = "a".repeat(63);
        let name = kubernetes_service_account("some-project", &app);
        assert!(
            name.len() <= 63 && name.starts_with(ACCOUNT_PREFIX),
            "{name}"
        );
        assert_eq!(name, kubernetes_service_account("some-project", &app));
        assert_ne!(name, kubernetes_service_account("other-project", &app));
    }
}
