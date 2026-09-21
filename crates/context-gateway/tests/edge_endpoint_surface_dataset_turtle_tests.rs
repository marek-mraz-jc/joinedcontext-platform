//! Edge cases of `endpoint_surface::dataset_turtle` (T-2496, EP-27, EP-68, DS-08).
//!
//! **The contract.** The endpoint's DCAT record as Turtle, for a triple store or a partner's
//! connector: every string literal goes through `literal()`, every IRI position stays one inert
//! `<…>` reference whatever the manifest or the schema index wrote into it, and a non-public
//! endpoint names its grant document as `odrl:hasPolicy`.
//!
//! **Inputs.** The endpoint's and the space's title and description (manifest text), the schema
//! index (artifact file names, digests), and the public base.

use std::collections::BTreeMap;
use std::sync::Arc;

use context_gateway::handlers::endpoint_surface::dataset_turtle;
use context_gateway::resolver::{Endpoint, Space};
use jc_core::kinds::{Audience, Representation};
use serde_json::{json, Value};

const SLUG: &str = "k4y7pq2mzt6vhx3nbwrs5cjd8f";
const BASE: &str = "https://bb.example.sk";

fn endpoint(audience: Audience, title: &[(&str, &str)]) -> Endpoint {
    Endpoint {
        slug: SLUG.to_owned(),
        title: title
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect(),
        description: Default::default(),
        space: "ovzdusie".to_owned(),
        project: "ovzdusie".to_owned(),
        audience,
        allowed_projects: Vec::new(),
        representations: vec![Representation::NgsiLd, Representation::Mcp],
        rate_limit: None,
        file_limits: None,
        hidden_attributes: Default::default(),
        projection: None,
        base_path: format!("/api/endpoint/{SLUG}"),
        models: Vec::new(),
        view_mapping: None,
        policies: Vec::new(),
    }
}

fn space(endpoint: &Endpoint, title: &[(&str, &str)]) -> Space {
    Space {
        endpoint: Arc::new(endpoint.clone()),
        title: title
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect::<BTreeMap<_, _>>(),
        description: Default::default(),
        is_sandbox: false,
        default_locale: None,
    }
}

/// A schema index of one model with the given artifacts (file name → descriptor).
fn index(artifacts: Value) -> Value {
    json!({ "models": [{ "name": "AirQuality", "version": 1, "artifacts": artifacts }] })
}

fn turtle(endpoint: &Endpoint, index: &Value) -> String {
    dataset_turtle(endpoint, None, index, BASE)
}

/// Every `<…>` of the document, and every `"…"` literal, read the way a Turtle parser reads them:
/// `None` when an IRI holds what IRIREF forbids or a literal does not end.
fn terms(document: &str) -> Option<(Vec<String>, Vec<String>)> {
    let (mut iris, mut literals) = (Vec::new(), Vec::new());
    let mut chars = document.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '<' => {
                let mut iri = String::new();
                loop {
                    match chars.next()? {
                        '>' => break,
                        c if c <= ' ' || "<\"{}|^`\\".contains(c) => return None,
                        c => iri.push(c),
                    }
                }
                iris.push(iri);
            }
            '"' => {
                let mut literal = String::new();
                loop {
                    match chars.next()? {
                        '"' => break,
                        '\\' => literal.push(match chars.next()? {
                            'n' => '\n',
                            'r' => '\r',
                            't' => '\t',
                            other => other,
                        }),
                        '\n' | '\r' => return None,
                        c => literal.push(c),
                    }
                }
                literals.push(literal);
            }
            '#' => return None,
            _ => {}
        }
    }
    Some((iris, literals))
}

/// The statements of the document: text between the terminating ` .` of each.
fn statements(document: &str) -> usize {
    document.matches(".\n").count()
}

/// EP-68: an artifact name cannot close its IRI and write a triple of its own.
#[test]
fn an_artifact_file_name_with_a_space_or_angle_bracket_does_not_inject_a_second_triple() {
    let clean = turtle(
        &endpoint(Audience::Public, &[]),
        &index(json!({ "air.schema.json": {} })),
    );
    for hostile in [
        "x.json> . <https://evil.example/s> <https://evil.example/p> <https://evil.example/o",
        "a b.json",
        "a\"b.json",
        "a\nb.json",
        "a{b}|c^d`e\\f.json",
        "a#b.json",
    ] {
        let document = turtle(
            &endpoint(Audience::Public, &[]),
            &index(json!({ hostile: {} })),
        );
        let (iris, _) = terms(&document).unwrap_or_else(|| panic!("{hostile:?}:\n{document}"));
        assert!(
            iris.iter()
                .all(|iri| !iri.contains("evil.example") || iri.contains("%3E")),
            "{hostile:?}:\n{document}"
        );
        assert_eq!(
            statements(&document),
            statements(&clean),
            "{hostile:?}:\n{document}"
        );
    }
}

/// EP-27: a title is a literal whatever it holds.
#[test]
fn a_title_with_a_quote_backslash_or_newline_is_escaped_and_does_not_break_the_document() {
    for title in [
        "Ovzdušie \"BB\"",
        "a\\b",
        "line\nbreak\r",
        "\\\"",
        "a\"; dct:title \"b",
    ] {
        let document = turtle(&endpoint(Audience::Public, &[("en", title)]), &json!({}));
        let (_, literals) = terms(&document).unwrap_or_else(|| panic!("{title:?}:\n{document}"));
        assert!(
            literals.iter().any(|l| l == title),
            "{title:?}: {literals:?}"
        );
        assert_eq!(
            document.matches("\n    dct:title ").count(),
            1,
            "{title:?}:\n{document}"
        );
    }
}

/// EP-68: a digest is a literal, so a quote in it cannot end the statement.
#[test]
fn a_sha256_checksum_containing_a_quote_is_escaped_as_a_literal() {
    let document = turtle(
        &endpoint(Audience::Public, &[]),
        &index(
            json!({ "air.schema.json": { "sha256": "ab\" ; a <https://evil.example/x> ; \"cd" } }),
        ),
    );
    let (iris, literals) = terms(&document).unwrap_or_else(|| panic!("{document}"));
    assert!(
        literals
            .iter()
            .any(|l| l.contains("https://evil.example/x")),
        "{literals:?}"
    );
    assert!(
        !iris.iter().any(|iri| iri.contains("evil.example")),
        "{iris:?}"
    );
}

/// DS-08: a public endpoint has no policy to dereference.
#[test]
fn a_public_endpoint_carries_no_odrl_haspolicy_triple() {
    let document = turtle(&endpoint(Audience::Public, &[]), &json!({}));
    assert!(!document.contains("odrl:hasPolicy"), "{document}");
    assert!(document.contains("access-right/PUBLIC"), "{document}");
}

/// DS-08: every other audience names its grant document.
#[test]
fn a_non_public_endpoint_always_carries_odrl_haspolicy() {
    for audience in [Audience::Organization, Audience::ProjectList] {
        let document = turtle(&endpoint(audience, &[]), &json!({}));
        assert!(
            document.contains(&format!(
                "odrl:hasPolicy <{BASE}/api/endpoint/{SLUG}/access>"
            )),
            "{audience:?}:\n{document}"
        );
        assert!(document.contains("access-right/RESTRICTED"), "{document}");
    }
}

/// EP-27: no schema, or an index that is not one, still renders the representations.
#[test]
fn an_endpoint_with_no_schema_artifacts_still_produces_valid_turtle() {
    for index in [
        json!({}),
        json!({ "models": [] }),
        json!({ "models": "x" }),
        json!(null),
        json!({ "models": [{ "version": "one", "artifacts": {} }] }),
    ] {
        let document = turtle(&endpoint(Audience::Public, &[]), &index);
        assert!(terms(&document).is_some(), "{index}:\n{document}");
        assert!(document.contains("a dcat:Dataset"), "{index}");
        assert_eq!(
            document.matches("a dcat:Distribution").count(),
            2,
            "{index}:\n{document}"
        );
    }
}

/// EP-27: the route answers the Turtle representation as `text/turtle`.
#[test]
fn the_output_content_type_is_turtle() {
    use context_gateway::handlers::space_surface::{negotiate, Format};
    let format = negotiate(Some("text/turtle"));
    assert!(matches!(format, Format::Turtle));
    assert_eq!(format.media_type(), "text/turtle");
}

/// EP-27: an endpoint with no title takes the space's, then the space's name; a title that is
/// only blank counts as none.
#[test]
fn an_empty_title_falls_back_to_the_space_or_slug_name() {
    let untitled = endpoint(Audience::Public, &[]);
    let titled_space = space(&untitled, &[("en", "Ovzdušie")]);
    let document = dataset_turtle(&untitled, Some(&titled_space), &json!({}), BASE);
    assert!(document.contains("dct:title \"Ovzdušie\""), "{document}");

    let document = dataset_turtle(&untitled, None, &json!({}), BASE);
    assert!(document.contains("dct:title \"ovzdusie\""), "{document}");

    for blank in [&[("en", "")][..], &[("en", "  "), ("sk", "")][..]] {
        let document = turtle(&endpoint(Audience::Public, blank), &json!({}));
        assert!(
            document.contains("dct:title \"ovzdusie\""),
            "{blank:?}:\n{document}"
        );
    }
}

/// EP-68: one access URL is one distribution, however many index entries name it.
#[test]
fn two_distributions_with_the_same_access_url_do_not_duplicate_the_dataset_level_triple() {
    let index = json!({ "models": [
        { "name": "A", "version": 1, "artifacts": { "shared.schema.json": {} } },
        { "name": "B", "version": 1, "artifacts": { "shared.schema.json": {} } },
    ] });
    let document = turtle(&endpoint(Audience::Public, &[]), &index);
    let url = format!("{BASE}/api/endpoint/{SLUG}/schema/v1/shared.schema.json");
    assert_eq!(
        document
            .matches(&format!("dcat:distribution <{url}>"))
            .count(),
        1,
        "{document}"
    );
    assert_eq!(
        document
            .matches(&format!("<{url}> a dcat:Distribution"))
            .count(),
        1,
        "{document}"
    );
}

/// EP-27: a title outside ASCII is written as it is, not mangled into escapes.
#[test]
fn unicode_in_the_title_round_trips_through_the_literal_escape_unchanged() {
    for title in [
        "Ovzdušie v Banskej Bystrici",
        "Luftqualität",
        "空気",
        "🌫️ smog",
    ] {
        let document = turtle(&endpoint(Audience::Public, &[("en", title)]), &json!({}));
        assert!(
            document.contains(&format!("dct:title \"{title}\"")),
            "{title}:\n{document}"
        );
    }
}

/// R20: what the index carries beyond the three fields the record reads stays out of it.
#[test]
fn no_secret_or_internal_hostname_appears_in_the_rendered_document() {
    let index = index(json!({ "air.schema.json": {
        "sha256": "abc",
        "source": "http://broker.internal.svc:1026/ngsi-ld/v1/types",
        "token": "eyJhbGciOiJSUzI1NiJ9.secret",
    } }));
    let document = turtle(&endpoint(Audience::Organization, &[]), &index);
    for word in ["internal", "1026", "eyJ", "secret"] {
        assert!(!document.contains(word), "{word}:\n{document}");
    }
}
