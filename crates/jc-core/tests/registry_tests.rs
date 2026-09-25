//! T-0002: the kind catalogue the Portal resource API and `jcctl` resolve paths through.

use jc_core::envelope::Scope;
use jc_core::registry::{by_kind, by_plural, KINDS};

#[test]
fn kind_and_plural_lookups_agree_and_are_unique() {
    let mut kinds: Vec<&str> = KINDS.iter().map(|k| k.kind).collect();
    let mut plurals: Vec<&str> = KINDS.iter().map(|k| k.plural).collect();
    kinds.sort_unstable();
    plurals.sort_unstable();
    let unique_kinds = {
        let mut c = kinds.clone();
        c.dedup();
        c.len()
    };
    assert_eq!(unique_kinds, kinds.len(), "duplicate kind name in KINDS");
    // Every kind has a plural of its own, so a route never has to guess the kind (R19, T-2954).
    plurals.dedup();
    assert_eq!(plurals.len(), KINDS.len(), "two kinds share a plural");

    for info in KINDS {
        assert_eq!(by_kind(info.kind), Some(info));
        assert_eq!(by_plural(info.plural).map(|k| k.plural), Some(info.plural));
    }
    assert!(by_kind("NoSuchKind").is_none());
    assert!(by_plural("nosuchplural").is_none());
}

/// R19, MF-11: ScopeDefinition answers under `scopedefinitions` and `policies` is Policy alone,
/// while the ScopeDefinition file stays where it always was.
#[test]
fn scope_definition_has_its_own_plural_and_keeps_its_path() {
    let scope = by_plural("scopedefinitions").expect("scopedefinitions catalogued");
    assert_eq!(scope.kind, "ScopeDefinition");
    assert_eq!(
        scope.repo_path("bb-ovzdusie", "", "geo-sk-bb"),
        "projects/bb-ovzdusie/policies/geo-sk-bb.yaml"
    );
    assert_eq!(by_plural("policies").map(|k| k.kind), Some("Policy"));
}

#[test]
fn context_space_plural_is_spaces() {
    // The Portal API and DEMO.md use /api/v1/projects/{project}/spaces.
    let cs = by_kind("ContextSpace").expect("ContextSpace catalogued");
    assert_eq!(cs.plural, "spaces");
    assert_eq!(cs.scope, Scope::Project);
    assert_eq!(
        cs.repo_path("bb-ovzdusie", "", "ovzdusie"),
        "projects/bb-ovzdusie/spaces/ovzdusie/space.yaml"
    );
}

#[test]
fn path_templates_render_the_documented_repository_layout() {
    let cases = [
        ("Organization", "", "", "org", "org.yaml"),
        (
            "Project",
            "",
            "",
            "bb-ovzdusie",
            "projects/bb-ovzdusie/project.yaml",
        ),
        (
            "Endpoint",
            "bb-ovzdusie",
            "ovzdusie",
            "air-quality-public",
            "projects/bb-ovzdusie/spaces/ovzdusie/endpoints/air-quality-public.yaml",
        ),
        (
            "Policy",
            "bb-ovzdusie",
            "ovzdusie",
            "public-air-quality",
            "projects/bb-ovzdusie/spaces/ovzdusie/policies/public-air-quality.yaml",
        ),
        (
            "DataModel",
            "bb-ovzdusie",
            "ovzdusie",
            "air-quality",
            "projects/bb-ovzdusie/spaces/ovzdusie/datamodels/air-quality.yaml",
        ),
        (
            "Mapping",
            "bb-ovzdusie",
            "ovzdusie",
            "sdm-to-bb",
            "projects/bb-ovzdusie/spaces/ovzdusie/datamodels/mappings/sdm-to-bb.yaml",
        ),
        (
            "ServiceAccount",
            "bb-ovzdusie",
            "",
            "ingest",
            "projects/bb-ovzdusie/access/serviceaccounts/ingest.yaml",
        ),
        (
            "Pipeline",
            "bb-ovzdusie",
            "",
            "loxone-mqtt",
            "projects/bb-ovzdusie/pipelines/loxone-mqtt/pipeline.yaml",
        ),
        (
            "App",
            "bb-ovzdusie",
            "",
            "air-map",
            "projects/bb-ovzdusie/apps/air-map/app.yaml",
        ),
        (
            "SharedSpaceReference",
            "bb-doprava",
            "",
            "external-air-quality",
            "projects/bb-doprava/shared/external-air-quality.yaml",
        ),
    ];

    for (kind, project, space, name, expected) in cases {
        let info = by_kind(kind).unwrap_or_else(|| panic!("{kind} catalogued"));
        assert_eq!(info.repo_path(project, space, name), expected, "{kind}");
        assert!(
            !info.repo_path(project, space, name).contains('{'),
            "{kind} left a placeholder unrendered"
        );
    }
}
