//! A pipeline mints ids of the space it runs in: `env("JC_SPACE")` is rendered per stream,
//! and a mapping that types its space in is a finding (PL-57, CC-83, T-1445).

use jcctl::bento::{inject_space, literal_names, space_var, SPACE_VAR};
use jcctl::commands::validate;
use serde_json::json;
use std::path::{Path, PathBuf};

const MAPPING: &str = r#"let domain = env("JC_ORG_DOMAIN")
root.id = "urn:ngsi-ld:%v:%v:%v:%v".format("BikeHireDockingStation", $domain, env("JC_SPACE"), this.id)"#;

#[test]
fn a_rendered_stream_carries_the_target_space_segment() {
    let mut stream = json!({ "pipeline": { "processors": [ { "mapping": MAPPING } ] } });
    inject_space(&mut stream, &["helsinki-bikes".to_owned()]);
    let mapping = stream["pipeline"]["processors"][0]["mapping"]
        .as_str()
        .unwrap();
    assert!(
        mapping.contains(r#"$domain, "helsinki-bikes", this.id"#),
        "{mapping}"
    );
    assert!(!mapping.contains(SPACE_VAR), "{mapping}");
    assert!(
        mapping.contains(r#"env("JC_ORG_DOMAIN")"#),
        "the domain stays the runner's: {mapping}"
    );
}

#[test]
fn a_pipeline_with_two_outputs_gets_one_variable_per_output() {
    assert_eq!(space_var(0), "JC_SPACE");
    assert_eq!(space_var(1), "JC_SPACE_2");
    let mut stream = json!([
        r#"root.a = env("JC_SPACE")"#,
        r#"root.b = env("JC_SPACE_2")"#,
        "${JC_SPACE_2}/${JC_SPACE}",
    ]);
    inject_space(
        &mut stream,
        &["helsinki-bikes".to_owned(), "helsinki-kpi".to_owned()],
    );
    assert_eq!(stream[0], r#"root.a = "helsinki-bikes""#);
    assert_eq!(stream[1], r#"root.b = "helsinki-kpi""#);
    assert_eq!(stream[2], "helsinki-kpi/helsinki-bikes");
}

#[test]
fn a_variable_with_no_output_behind_it_is_left_alone() {
    let mut stream = json!(r#"root.b = env("JC_SPACE_2")"#);
    inject_space(&mut stream, &["helsinki-bikes".to_owned()]);
    assert_eq!(stream, r#"root.b = env("JC_SPACE_2")"#);
    let mut empty = json!({});
    inject_space(&mut empty, &[]);
    assert_eq!(empty, json!({}));
}

#[test]
fn a_literal_space_is_found_and_a_word_inside_a_url_or_title_is_not() {
    let bento = "# \"helsinki\" in a comment is prose\n\
                 - mapping: |\n\
                 \x20   root.url = \"https://helsinki.fi/data\"\n\
                 \x20   root.title = \"Helsinki bikes\"\n\
                 \x20   root.id = [\"X\", $domain, \"helsinki\", this.id].join(\":\")\n";
    assert_eq!(
        literal_names(bento, &["helsinki"]),
        vec![(5, "helsinki".to_owned())]
    );
    assert!(literal_names(MAPPING, &["helsinki", "helsinki-bikes"]).is_empty());
    assert!(literal_names(bento, &[]).is_empty());
}

fn write(dir: &Path, rel: &str, body: &str) {
    let path = dir.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
}

fn repo(test: &str, bento: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("jcctl-bento-env-{test}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    write(&dir, "org.yaml", "apiVersion: joinedcontext.com/v1alpha1\nkind: Organization\nmetadata:\n  name: helsinki\n  namespace: org\nspec:\n  domain: hel.fi\n  locales: [\"en\"]\n  defaultLocale: en\n");
    write(&dir, "projects/helsinki/project.yaml", "apiVersion: joinedcontext.com/v1alpha1\nkind: Project\nmetadata:\n  name: helsinki\n  namespace: org\nspec:\n  organizationRef: helsinki\n");
    write(&dir, "projects/helsinki/spaces/hub/space.yaml", "apiVersion: joinedcontext.com/v1alpha1\nkind: ContextSpace\nmetadata:\n  name: hub\n  namespace: helsinki\nspec:\n  urnSegment: helsinki-hub\n");
    write(&dir, "projects/helsinki/pipelines/bikes/pipeline.yaml", "apiVersion: joinedcontext.com/v1alpha1\nkind: Pipeline\nmetadata:\n  name: bikes\n  namespace: helsinki\nspec:\n  class: auto\n  targetEndpoint: urn:ngsi-ld:Endpoint:{orgDomain}:helsinki-hub:all\n");
    write(&dir, "projects/helsinki/pipelines/bikes/bento.yaml", bento);
    dir
}

#[test]
fn validate_names_the_file_the_line_and_the_replacement() {
    let dir = repo("literal", "pipeline:\n  processors:\n    - mapping: |\n        root.id = [\"X\", \"helsinki-hub\", this.id].join(\":\")\n");
    let report = validate::run(&dir);
    let warning = report
        .warnings
        .iter()
        .find(|w| w.message.contains("types the space"))
        .unwrap_or_else(|| panic!("a finding: {:?}", report.warnings));
    assert_eq!(
        warning.path,
        PathBuf::from("projects/helsinki/pipelines/bikes/bento.yaml")
    );
    assert_eq!(warning.line, 4);
    assert!(
        warning.message.contains("\"helsinki-hub\""),
        "{}",
        warning.message
    );
    assert!(
        warning.message.contains("env(\"JC_SPACE\")"),
        "{}",
        warning.message
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_same_mapping_with_the_variable_is_clean() {
    let dir = repo(
        "clean",
        &format!(
            "pipeline:\n  processors:\n    - mapping: |\n{}\n",
            MAPPING
                .lines()
                .map(|l| format!("        {l}"))
                .collect::<Vec<_>>()
                .join("\n")
        ),
    );
    let report = validate::run(&dir);
    assert!(
        report
            .warnings
            .iter()
            .all(|w| !w.message.contains("types the space")),
        "{:?}",
        report.warnings
    );
    let _ = std::fs::remove_dir_all(&dir);
}
