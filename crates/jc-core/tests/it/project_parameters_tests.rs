//! A project's deployment parameters: declared in the project file, set on the registry entry,
//! rendered into manifests and mappings (T-2639, CC-88, CC-83).

use jc_core::kinds::Project;
use jc_core::project::{self, Parameters};
use serde_json::json;

fn file(parameters: &str) -> Project {
    let project = Project::from_yaml(&format!(
        "apiVersion: joinedcontext.com/v1alpha1\nkind: Project\n\
         metadata: {{ name: air, namespace: org }}\n\
         spec:\n  organizationRef: {{ kind: Organization, name: helsinki }}\n  version: 1.4.0\n\
         \x20 parameters:\n{parameters}"
    ))
    .expect("the project file parses");
    project.validate().expect("the project file is valid");
    project
}

fn entry(values: &str) -> Project {
    let project = Project::from_yaml(&format!(
        "apiVersion: joinedcontext.com/v1alpha1\nkind: Project\n\
         metadata: {{ name: air, namespace: org }}\n\
         spec:\n  organizationRef: {{ kind: Organization, name: helsinki }}\n\
         \x20 repository: {{ name: air }}\n  ref: v1.4.0\n  parameters: {values}\n"
    ))
    .expect("the entry parses");
    project.validate().expect("the entry is valid");
    project
}

const DECLARED: &str = "    stationCount: { type: integer, default: 8 }\n    \
                        region: { type: string, default: uusimaa, enum: [uusimaa, pirkanmaa] }\n    \
                        feedHost: { type: string, default: data.hel.fi }\n    \
                        live: { type: boolean, default: true }\n    \
                        ingestToken: { type: secret, default: air-ingest-token }\n";

fn resolved(values: &str) -> Parameters {
    project::resolve_parameters(&file(DECLARED).spec, &entry(values).spec)
        .expect("the parameters resolve")
}

/// CC-88: a value the entry sets wins over the default, and every other parameter keeps its
/// default.
#[test]
fn values_come_from_the_entry_over_the_defaults() {
    let parameters = resolved("{ stationCount: 12, region: pirkanmaa }");
    let mut manifest = json!({
        "count": "{param:stationCount}",
        "region": "{param:region}",
        "host": "{param:feedHost}",
        "enabled": "{param:live}",
        "url": "https://{param:feedHost}/v2/{param:region}?limit={param:stationCount}",
    });
    parameters.render(&mut manifest).expect("renders");
    assert_eq!(
        manifest,
        json!({
            "count": 12,
            "region": "pirkanmaa",
            "host": "data.hel.fi",
            "enabled": true,
            "url": "https://data.hel.fi/v2/pirkanmaa?limit=12",
        })
    );
}

/// CC-88: a secret parameter renders as the name of a secretRef, never a value, and only where
/// it stands alone; the error never repeats what the entry set.
#[test]
fn a_secret_parameter_renders_a_secret_ref_name() {
    let parameters = resolved("{ ingestToken: air-ingest-token-staging }");
    let mut manifest = json!({ "spec": { "tokenRef": { "name": "{param:ingestToken}" } } });
    parameters.render(&mut manifest).expect("renders");
    assert_eq!(
        manifest["spec"]["tokenRef"]["name"],
        "air-ingest-token-staging"
    );

    let mut embedded = json!({ "spec": { "url": "https://x/?t={param:ingestToken}" } });
    let message = parameters
        .render(&mut embedded)
        .expect_err("refused")
        .to_string();
    assert!(
        message.contains("ingestToken") && message.contains("spec.url"),
        "{message}"
    );
    assert!(!message.contains("air-ingest-token-staging"), "{message}");

    assert!(!parameters
        .env()
        .values()
        .any(|value| value.contains("air-ingest")));
}

/// CC-88, the security property: a value of a secret parameter that is not a secret name is
/// refused, and the refusal does not carry it.
#[test]
fn a_secret_value_never_appears_in_a_refusal() {
    for pasted in [
        "glpat-abcdefghijklmnopqrst",
        "hunter2 with spaces",
        "Not_A_Label",
    ] {
        let failure = project::resolve_parameters(
            &file(DECLARED).spec,
            &entry(&format!("{{ ingestToken: \"{pasted}\" }}")).spec,
        );
        let message = failure.expect_err("refused").to_string();
        assert!(message.contains("ingestToken"), "{message}");
        assert!(!message.contains(pasted), "{message}");
    }
}

/// CC-88: an entry naming a parameter the project does not declare is refused by name, and so
/// is a value of the wrong type, one outside the enum, and a declared parameter nobody sets.
#[test]
fn an_undeclared_mistyped_or_missing_parameter_is_refused() {
    let declared = file(DECLARED);
    let refuse = |values: &str| {
        project::resolve_parameters(&declared.spec, &entry(values).spec)
            .expect_err("refused")
            .to_string()
    };
    assert!(refuse("{ stationCnt: 12 }").contains("stationCnt"));
    let mistyped = refuse("{ stationCount: twelve }");
    assert!(
        mistyped.contains("stationCount") && mistyped.contains("integer"),
        "{mistyped}"
    );
    assert!(refuse("{ region: lappi }").contains("region"));
    assert!(refuse("{ live: \"yes\" }").contains("boolean"));

    let without_default = file("    stationCount: { type: integer }\n");
    let message = project::resolve_parameters(&without_default.spec, &entry("{}").spec)
        .expect_err("refused")
        .to_string();
    assert!(message.contains("stationCount"), "{message}");
}

/// CC-88: a declaration refuses a default of the wrong type, a default outside its enum, a name
/// that is not an identifier, and an enum on a secret.
#[test]
fn a_declaration_is_checked_where_it_is_written() {
    for (declaration, named) in [
        (
            "    stationCount: { type: integer, default: eight }\n",
            "stationCount",
        ),
        (
            "    region: { type: string, default: lappi, enum: [uusimaa] }\n",
            "region",
        ),
        (
            "    station-count: { type: integer, default: 8 }\n",
            "station-count",
        ),
        ("    token: { type: secret, enum: [a, b] }\n", "token"),
        (
            "    token: { type: secret, default: \"glpat-abcdefghijklmnopqrst\" }\n",
            "token",
        ),
    ] {
        let project = Project::from_yaml(&format!(
            "apiVersion: joinedcontext.com/v1alpha1\nkind: Project\n\
             metadata: {{ name: air, namespace: org }}\n\
             spec:\n  organizationRef: {{ kind: Organization, name: helsinki }}\n\
             \x20 parameters:\n{declaration}"
        ))
        .expect("parses");
        let message = project.validate().expect_err("refused").to_string();
        assert!(message.contains(named), "{declaration}: {message}");
        assert!(!message.contains("glpat-"), "{message}");
    }
}

/// CC-88: a placeholder naming no parameter is refused with where it stands.
#[test]
fn an_unknown_placeholder_is_refused_with_its_path() {
    let parameters = resolved("{}");
    let mut manifest = json!({ "spec": { "sources": [ { "host": "{param:feedHots}" } ] } });
    let message = parameters
        .render(&mut manifest)
        .expect_err("refused")
        .to_string();
    assert!(message.contains("feedHots"), "{message}");
    assert!(message.contains("spec.sources[0].host"), "{message}");
}

/// CC-88: a mapping reads a parameter as `env("JC_PARAM_<NAME>")`; the runner receives one
/// variable per non-secret parameter, and a mapping reading one that is not there is named.
#[test]
fn a_mapping_reads_parameters_from_its_environment() {
    let parameters = resolved("{ stationCount: 12 }");
    let env = parameters.env();
    assert_eq!(
        env.get("JC_PARAM_STATION_COUNT").map(String::as_str),
        Some("12")
    );
    assert_eq!(
        env.get("JC_PARAM_REGION").map(String::as_str),
        Some("uusimaa")
    );
    assert_eq!(env.get("JC_PARAM_LIVE").map(String::as_str), Some("true"));
    assert!(!env.contains_key("JC_PARAM_INGEST_TOKEN"));
    assert_eq!(project::env_name("feedHost"), "JC_PARAM_FEED_HOST");

    let mapping = r#"root.count = env("JC_PARAM_STATION_COUNT").number()
root.region = env( "JC_PARAM_REGION" )
root.token = env("JC_PARAM_INGEST_TOKEN")
root.typo = env("JC_PARAM_STATON_COUNT")"#;
    assert_eq!(
        parameters.unknown_in_mapping(mapping),
        ["JC_PARAM_INGEST_TOKEN", "JC_PARAM_STATON_COUNT"]
    );
}

/// CC-83 extended by CC-88: a project file carrying the literal value of a declared parameter is
/// reported with the path and the placeholder to write instead; a word that merely contains
/// it, a number and a secret's name are not findings.
#[test]
fn a_literal_where_a_parameter_exists_is_named_with_its_replacement() {
    let parameters = resolved("{ region: pirkanmaa }");
    let manifest = json!({
        "spec": {
            "host": "data.hel.fi",
            "labels": ["pirkanmaa", "uusimaa"],
            "title": "Air in uusimaa",
            "limit": 8,
            "tokenRef": { "name": "air-ingest-token" },
            "already": "{param:feedHost}",
        }
    });
    let findings = parameters.literals(&manifest);
    let named: Vec<(&str, &str)> = findings
        .iter()
        .map(|found| (found.path.as_str(), found.placeholder.as_str()))
        .collect();
    assert_eq!(
        named,
        [
            ("spec.host", "{param:feedHost}"),
            ("spec.labels[0]", "{param:region}"),
            ("spec.labels[1]", "{param:region}"),
        ]
    );
}

/// CC-88: a project with no parameters renders a manifest unchanged, and an entry setting none
/// is not refused.
#[test]
fn a_project_without_parameters_renders_unchanged() {
    let bare = Project::from_yaml(
        "apiVersion: joinedcontext.com/v1alpha1\nkind: Project\n\
         metadata: { name: air, namespace: org }\n\
         spec:\n  organizationRef: { kind: Organization, name: helsinki }\n",
    )
    .expect("parses");
    let parameters = project::resolve_parameters(&bare.spec, &entry("{}").spec).expect("resolves");
    let original = json!({ "spec": { "host": "{orgDomain}", "n": 3 } });
    let mut manifest = original.clone();
    parameters.render(&mut manifest).expect("renders");
    assert_eq!(manifest, original);
    assert!(parameters.env().is_empty());
}
