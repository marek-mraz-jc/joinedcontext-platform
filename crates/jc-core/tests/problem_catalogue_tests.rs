//! Every error the platform answers with is a problem type of the catalogue (T-3243, API/00 §4):
//! a title, a status, and a hint that tells the caller what to do; a validation names its field.
//! The source walk is the audit: a `ProblemDetails::new` with a slug outside the catalogue, or a
//! slug built at run time, fails here before it reaches a client.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use jc_core::error::{problem_type, Error, ProblemDetails, PROBLEM_TYPES};
use serde_json::json;

fn sources(dir: &Path, found: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("a readable source directory") {
        let path = entry.expect("an entry").path();
        if path.is_dir() {
            sources(&path, found);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            found.push(path);
        }
    }
}

/// The second argument of every `ProblemDetails::new(` in the workspace's crates' `src/`.
fn slugs_in_source() -> Vec<(String, String)> {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut files = Vec::new();
    for krate in std::fs::read_dir(&crates).expect("the crates directory") {
        let src = krate.expect("a crate").path().join("src");
        if src.is_dir() {
            sources(&src, &mut files);
        }
    }
    let mut found = Vec::new();
    for file in files {
        let text = std::fs::read_to_string(&file).expect("a readable source file");
        for (at, _) in text.match_indices("ProblemDetails::new(") {
            let rest = &text[at + "ProblemDetails::new(".len()..];
            // The definition itself takes `slug: &str`; it is no call.
            if rest.trim_start().starts_with("status: u16") {
                continue;
            }
            let second = rest.split(',').nth(1).unwrap_or_default().trim();
            found.push((file.display().to_string(), second.to_owned()));
        }
    }
    found
}

#[test]
fn every_problem_the_platform_writes_is_a_catalogued_type() {
    let found = slugs_in_source();
    assert!(found.len() > 20, "the walk found the calls: {found:?}");
    let unknown: Vec<String> = found
        .iter()
        .filter(|(_, slug)| {
            slug.strip_prefix('"')
                .and_then(|slug| slug.strip_suffix('"'))
                .is_none_or(|slug| problem_type(slug).is_none())
        })
        .map(|(file, slug)| format!("{file}: {slug}"))
        .collect();
    assert!(
        unknown.is_empty(),
        "a slug that is not a literal of PROBLEM_TYPES (add it to the catalogue and API/00 §4):\n  {}",
        unknown.join("\n  ")
    );
}

#[test]
fn every_type_has_a_title_a_status_and_a_hint_a_person_can_act_on() {
    let mut seen = BTreeSet::new();
    for known in PROBLEM_TYPES {
        assert!(seen.insert(known.slug), "{} is listed twice", known.slug);
        assert!(
            known
                .slug
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b == b'-'),
            "{}",
            known.slug
        );
        assert!(!known.title.is_empty(), "{}", known.slug);
        assert!(
            known.status == 0 || (400..600).contains(&known.status),
            "{}",
            known.slug
        );
        assert!(
            known.hint.len() > 20 && known.hint.ends_with('.'),
            "{}: a hint is one sentence",
            known.slug
        );
        // Never an internal name: no crate, no module path, no stack frame.
        for internal in ["::", "crate", "src/", "panic", "unwrap"] {
            assert!(
                !known.hint.contains(internal),
                "{}: {}",
                known.slug,
                known.hint
            );
        }
        let problem = ProblemDetails::new(known.status.max(500), known.slug, known.title);
        assert_eq!(problem.extensions.get("hint"), Some(&json!(known.hint)));
    }
}

#[test]
fn a_status_alone_gets_the_platforms_type_never_one_made_of_its_reason() {
    for status in [
        400, 401, 403, 404, 405, 409, 412, 413, 415, 418, 422, 429, 500, 501, 502, 503, 504,
    ] {
        let problem = ProblemDetails::for_status(status);
        assert_eq!(problem.status, status);
        let slug = problem.type_uri.rsplit('/').next().expect("a slug");
        assert!(problem_type(slug).is_some(), "{status}: {slug}");
        assert!(problem.extensions.contains_key("hint"), "{status}");
    }
    assert!(ProblemDetails::for_status(404)
        .type_uri
        .ends_with("/resource-not-found"));
    assert!(ProblemDetails::for_status(422)
        .type_uri
        .ends_with("/bad-request"));
    assert!(ProblemDetails::for_status(501)
        .type_uri
        .ends_with("/internal-error"));
}

#[test]
fn a_validation_names_the_field_it_refused() {
    let named: ProblemDetails = Error::Name {
        field: "metadata.name",
        value: "Bad Name".into(),
        reason: "lowercase letters, digits and dashes",
    }
    .into();
    assert_eq!(named.extensions.get("field"), Some(&json!("metadata.name")));
    let invalid: ProblemDetails = Error::Invalid {
        field: "spec.slug".into(),
        reason: "too long".into(),
    }
    .into();
    assert_eq!(invalid.extensions.get("field"), Some(&json!("spec.slug")));
    let urn: ProblemDetails = Error::Urn {
        urn: "urn:x".into(),
        reason: jc_core::UrnError::InvalidPrefix,
    }
    .into();
    assert_eq!(urn.extensions.get("field"), Some(&json!("id")));
    let kind: ProblemDetails = Error::Kind {
        expected: "Endpoint",
        got: "Endpoints".into(),
    }
    .into();
    assert_eq!(kind.extensions.get("field"), Some(&json!("kind")));
    // A parse failure names no single field; it says where in the text instead.
    let parse: ProblemDetails = Error::Parse("expected a mapping at line 3".into()).into();
    assert!(!parse.extensions.contains_key("field"));
    assert!(parse.extensions.contains_key("hint"));
}
