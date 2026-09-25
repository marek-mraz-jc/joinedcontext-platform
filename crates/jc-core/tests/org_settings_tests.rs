//! T-2715, T-2716: `Organization.spec.policies` and `spec.limits` (ADR-N-035, PF-96, PF-97).

use jc_core::kinds::org_settings::{entry_at, Bound, Origin, CATALOG};
use jc_core::kinds::{Organization, OrganizationBounds, PublicApps};

const ORG: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: Organization
metadata:
  name: helsinki
  namespace: org
spec:
  domain: hel.fi
  locales: [fi, en]
  defaultLocale: fi
"#;

fn org(spec_tail: &str) -> Result<Organization, String> {
    let manifest =
        Organization::from_yaml(&format!("{ORG}{spec_tail}")).map_err(|e| e.to_string())?;
    manifest.validate().map_err(|e| e.to_string())?;
    Ok(manifest)
}

fn entry(path: &str) -> &'static jc_core::kinds::org_settings::Entry {
    entry_at(path).unwrap_or_else(|| panic!("{path} is catalogued"))
}

fn bounds(pairs: &[(&str, Option<u32>, Option<u32>)]) -> OrganizationBounds {
    OrganizationBounds(
        pairs
            .iter()
            .map(|(path, min, max)| {
                (
                    (*path).to_owned(),
                    Bound {
                        min: *min,
                        max: *max,
                    },
                )
            })
            .collect(),
    )
}

#[test]
fn an_absent_entry_is_its_default_and_a_set_one_is_the_organizations() {
    let bare = org("").expect("a bare organization");
    assert_eq!(bare.spec.policies.apps.public, PublicApps::Allowed);
    assert_eq!(
        bare.spec
            .effective(entry("spec.limits.edge.requestsPerMinute.web")),
        (Some(300), Origin::Default)
    );
    assert_eq!(
        bare.spec.effective(entry("spec.limits.agents.spendPerDay")),
        (None, Origin::Default),
        "no spend limit unless one is set"
    );
    let yaml = serde_norway::to_string(&bare.spec).expect("serialize");
    assert!(
        !yaml.contains("policies") && !yaml.contains("limits"),
        "{yaml}"
    );

    let set = org(
        "  policies:\n    apps: { public: refused }\n  limits:\n    edge:\n      requestsPerMinute: { web: 120 }\n",
    )
    .expect("a lowered rate");
    assert_eq!(set.spec.policies.apps.public, PublicApps::Refused);
    assert_eq!(
        set.spec
            .effective(entry("spec.limits.edge.requestsPerMinute.web")),
        (Some(120), Origin::Organization)
    );
    assert_eq!(
        set.spec
            .effective(entry("spec.limits.edge.requestsPerMinute.api")),
        (Some(1200), Origin::Default)
    );
}

#[test]
fn an_unknown_field_or_value_is_refused() {
    for tail in [
        "  limits:\n    edge: { rps: 3 }\n",
        "  limits:\n    edge:\n      requestsPerMinute: { webs: 3 }\n",
        "  policies:\n    apps: { public: maybe }\n",
        "  policies:\n    signIn: { mfa: required }\n",
        "  limits:\n    people: { invitationHours: -1 }\n",
    ] {
        assert!(org(tail).is_err(), "{tail}");
    }
}

#[test]
fn a_security_floor_and_the_built_in_ranges_hold_without_an_operator_file() {
    let err =
        org("  policies:\n    signIn:\n      password: { minLength: 8 }\n").expect_err("below 12");
    assert!(
        err.contains("spec.policies.signIn.password.minLength") && err.contains("at least 12"),
        "{err}"
    );
    let err = org("  limits:\n    signIn: { sessionMaxHours: 48 }\n").expect_err("over 24 h");
    assert!(err.contains("1 … 24"), "{err}");
    let err = org("  limits:\n    edge:\n      requestsPerMinute: { web: 0 }\n").expect_err("zero");
    assert!(err.contains("requestsPerMinute.web"), "{err}");
    // A number's built-in ceiling is the operator's to raise, so parsing alone lets it pass.
    org("  limits:\n    edge:\n      requestsPerMinute: { web: 5000 }\n")
        .expect("the operator decides");
}

#[test]
fn a_body_larger_than_the_edges_and_a_session_idling_past_its_life_are_refused() {
    let err =
        org("  limits:\n    gateway: { maxRequestBodyMegabytes: 20 }\n").expect_err("over 16");
    assert!(
        err.contains("gateway.maxRequestBodyMegabytes") && err.contains("16 MiB"),
        "{err}"
    );
    org(
        "  limits:\n    edge: { maxRequestBodyMegabytes: 32 }\n    data: { uploadMegabytes: 32 }\n",
    )
    .expect("a larger edge admits a larger upload");
    let err =
        org("  limits:\n    data: { uploadMegabytes: 17 }\n").expect_err("over the default edge");
    assert!(err.contains("data.uploadMegabytes"), "{err}");

    let err = org("  limits:\n    signIn: { sessionIdleMinutes: 180, sessionMaxHours: 2 }\n")
        .expect_err("idle past the end");
    assert!(err.contains("sessionIdleMinutes"), "{err}");
}

#[test]
fn an_empty_model_list_is_refused_and_an_absent_one_allows_every_model() {
    assert!(org("  policies:\n    agents: { models: [] }\n").is_err());
    assert!(org("  policies:\n    agents: { models: [\" \"] }\n").is_err());
    let listed =
        org("  policies:\n    agents: { models: [claude-sonnet-5] }\n").expect("one model");
    assert_eq!(
        listed.spec.policies.agents.models.as_deref(),
        Some(&["claude-sonnet-5".to_owned()][..])
    );
}

#[test]
fn the_operators_bound_replaces_the_built_in_one_and_the_refusal_names_it() {
    let web = "spec.limits.edge.requestsPerMinute.web";
    let raised =
        org("  limits:\n    edge:\n      requestsPerMinute: { web: 5000 }\n").expect("parses");
    let err = raised
        .spec
        .check_limits(&OrganizationBounds::default())
        .expect_err("over the built-in 3000");
    assert!(
        err.to_string().contains(web) && err.to_string().contains("1 … 3000"),
        "{err}"
    );
    raised
        .spec
        .check_limits(&bounds(&[(web, None, Some(6000))]))
        .expect("the operator raised it");
    let err = raised
        .spec
        .check_limits(&bounds(&[(web, None, Some(1000))]))
        .expect_err("the operator lowered it");
    assert!(
        err.to_string()
            .contains("5000 is outside the bound, 1 … 1000"),
        "{err}"
    );

    let quota = "spec.projects.quota.contextSpaces";
    let spaces =
        org("  projects:\n    quota: { contextSpaces: 40 }\n").expect("no built-in ceiling");
    spaces
        .spec
        .check_limits(&OrganizationBounds::default())
        .expect("no ceiling");
    assert!(spaces
        .spec
        .check_limits(&bounds(&[(quota, None, Some(20))]))
        .is_err());
}

#[test]
fn an_operator_file_names_catalogued_entries_and_never_loosens_a_security_one() {
    bounds(&[("spec.limits.edge.requestsPerMinute.web", None, Some(6000))])
        .validate()
        .expect("a number's ceiling is the operator's");
    bounds(&[("spec.limits.signIn.sessionMaxHours", None, Some(8))])
        .validate()
        .expect("tightening is allowed");
    for (path, min, max, why) in [
        ("spec.limits.edge.rps", None, Some(1), "no such entry"),
        (
            "spec.limits.signIn.sessionMaxHours",
            None,
            Some(48),
            "loosens a security ceiling",
        ),
        (
            "spec.policies.signIn.password.minLength",
            Some(8),
            None,
            "loosens a security floor",
        ),
        (
            "spec.limits.data.uploadMegabytes",
            Some(10),
            Some(5),
            "upside down",
        ),
    ] {
        assert!(bounds(&[(path, min, max)]).validate().is_err(), "{why}");
    }
}

#[test]
fn every_catalog_entry_is_readable_from_a_manifest_and_its_default_is_in_range() {
    for entry in CATALOG {
        if let Some(default) = entry.default {
            assert!(default >= entry.min, "{}", entry.path);
            assert!(entry.max.is_none_or(|max| default <= max), "{}", entry.path);
        }
    }
    let full = org(
        "  projects:\n    nameCooldownDays: 7\n    quota: { contextSpaces: 1, residentPipelines: 1, publicEndpoints: 1, ingestEventsPerSecond: 1, apps: 1, agentRunsPerDay: 1, entitiesPerSpace: 1, requestsPerMinute: 1 }\n  policies:\n    signIn:\n      password: { minLength: 13, history: 1 }\n  limits:\n    edge:\n      requestsPerMinute: { web: 1, api: 1, dataRead: 1, dataWrite: 1, publicEndpoint: 1 }\n      maxRequestBodyMegabytes: 1\n    gateway: { maxRequestBodyMegabytes: 1 }\n    signIn: { sessionIdleMinutes: 60, sessionMaxHours: 1 }\n    people: { invitationHours: 1 }\n    agents: { spendPerDay: 1, spendPerMonth: 1 }\n    pipelines: { rejectedKept: 1, sampleMegabytes: 1 }\n    data: { uploadMegabytes: 1 }\n",
    )
    .expect("every entry set");
    for entry in CATALOG {
        assert_eq!(
            full.spec.effective(entry).1,
            Origin::Organization,
            "{} is read from the manifest",
            entry.path
        );
    }
    let yaml = serde_norway::to_string(&full.spec).expect("serialize");
    let again: jc_core::kinds::OrganizationSpec =
        serde_norway::from_str(&yaml).expect("round trip");
    assert_eq!(again, full.spec);
}
