//! T-3583: the three layers of an App's platform services, the service lists and the per-App
//! quotas (AP-163, AP-165, ADR-N-045).

use jc_core::kinds::org_settings::entry_at;
use jc_core::kinds::{
    AppService, AppServiceLimits, Organization, Project, DEFAULT_ORGANIZATION_SERVICES,
};

const ORG: &str = "apiVersion: joinedcontext.com/v1alpha1
kind: Organization
metadata: { name: banskabystrica, namespace: org }
spec:
  domain: bbsk.sk
  locales: [sk]
  defaultLocale: sk
";

const PROJECT: &str = "apiVersion: joinedcontext.com/v1alpha1
kind: Project
metadata: { name: doprava, namespace: org }
spec:
  organizationRef: { kind: Organization, name: banskabystrica }
";

#[test]
fn each_layer_reads_its_services_and_quotas() {
    let org = Organization::from_yaml(&format!(
        "{ORG}  policies:\n    apps:\n      services: [identity, data, email]\n  limits:\n    apps: {{ emailsPerDay: 20, filesMiB: 64 }}\n"
    ))
    .expect("organization");
    org.validate().expect("valid organization");
    assert_eq!(
        org.spec.policies.apps.services.as_deref(),
        Some(&[AppService::Identity, AppService::Data, AppService::Email][..])
    );
    let path = |p| entry_at(p).expect("catalogued");
    assert_eq!(
        org.spec.effective(path("spec.limits.apps.emailsPerDay")).0,
        Some(20)
    );
    assert_eq!(
        org.spec
            .effective(path("spec.limits.apps.aiTokensPerDay"))
            .0,
        Some(200_000),
        "an absent quota is the catalog's"
    );

    let project = Project::from_yaml(&format!(
        "{PROJECT}  apps:\n    services: [email]\n    limits: {{ emailsPerDay: 5 }}\n"
    ))
    .expect("project");
    project.validate().expect("valid project");
    let apps = project.spec.apps.clone().expect("spec.apps");
    assert_eq!(apps.services, Some(vec![AppService::Email]));
    assert_eq!(
        apps.limits,
        AppServiceLimits {
            emails_per_day: Some(5),
            ..AppServiceLimits::default()
        }
    );
    let yaml = serde_norway::to_string(&project.spec).expect("serialize");
    assert!(!yaml.contains("aiTokensPerDay"), "{yaml}");
}

#[test]
fn an_absent_list_is_the_default_and_email_and_ai_are_off_by_default() {
    let org = Organization::from_yaml(ORG).expect("organization");
    assert_eq!(org.spec.policies.apps.services, None);
    assert!(!DEFAULT_ORGANIZATION_SERVICES.contains(&AppService::Email));
    assert!(!DEFAULT_ORGANIZATION_SERVICES.contains(&AppService::Ai));
}

#[test]
fn an_unknown_service_or_quota_is_refused() {
    for tail in [
        "  policies:\n    apps:\n      services: [sms]\n",
        "  limits:\n    apps: { filesMib: 1 }\n",
    ] {
        assert!(
            Organization::from_yaml(&format!("{ORG}{tail}")).is_err(),
            "{tail}"
        );
    }
    assert!(Project::from_yaml(&format!("{PROJECT}  apps:\n    emails: 1\n")).is_err());
}
