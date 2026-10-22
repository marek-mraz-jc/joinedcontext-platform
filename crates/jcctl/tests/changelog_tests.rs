//! The public changelog is read from the merged history of a project's manifests, and shows a
//! consumer what changed and nothing an author wrote for reviewers (EP-148, EP-149).

mod common;

use chrono::{Duration, Utc};
use common::{temp_dir, write, SPACE};
use jc_core::kinds::Representation;
use jcctl::commands::changelog::{entries, markdown, Options};
use std::path::{Path, PathBuf};
use std::process::Command;

const ENDPOINT_PATH: &str = "spaces/ovzdusie/endpoints/air.yaml";

fn git(dir: &Path, date: Option<&str>, args: &[&str]) {
    let mut command = Command::new("git");
    command
        .args([
            "-c",
            "user.name=Reviewer Name",
            "-c",
            "user.email=r@example.org",
            "-C",
        ])
        .arg(dir)
        .args(args);
    if let Some(date) = date {
        command
            .env("GIT_COMMITTER_DATE", date)
            .env("GIT_AUTHOR_DATE", date);
    }
    let output = command.output().expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn commit(dir: &Path, date: Option<&str>, message: &str) {
    git(dir, None, &["add", "-A"]);
    git(dir, date, &["commit", "-q", "-m", message]);
}

fn endpoint(name: &str, audience: &str, extra: &str) -> String {
    format!(
        "apiVersion: joinedcontext.com/v1alpha1\nkind: Endpoint\nmetadata:\n  name: {name}\n  \
         namespace: ovzdusie\n  title: {{ en: \"Air <b>quality</b>\", sk: \"Ovzdušie\" }}\nspec:\n  \
         contextSpaceRef: ovzdusie\n  slug: zt4qm7ge2xdv6ksb3ncf5arw2y\n  audience: {audience}\n{extra}"
    )
}

/// A project whose public Endpoint lives through the window: published, an internal edit, a
/// surface, a merged branch hiding an attribute, a deprecation, then no longer public. An
/// organization Endpoint and a change older than the window stay out.
fn repository() -> PathBuf {
    let dir = temp_dir("changelog");
    git(&dir, None, &["init", "-q", "-b", "main"]);
    let old = (Utc::now() - Duration::days(60)).to_rfc3339();
    write(&dir, "spaces/ovzdusie/space.yaml", SPACE);
    write(
        &dir,
        "spaces/ovzdusie/endpoints/old.yaml",
        &endpoint("old-air", "public", "")
            .replace("zt4qm7ge2xdv6ksb3ncf5arw2y", "ab4qm7ge2xdv6ksb3ncf5arw2y"),
    );
    commit(&dir, Some(&old), "old publication");
    write(
        &dir,
        "spaces/ovzdusie/endpoints/internal.yaml",
        &endpoint("internal-air", "organization", ""),
    );
    commit(&dir, None, "internal endpoint");
    let reps = "  enabledRepresentations: [ngsi-ld, csv]\n";
    write(&dir, ENDPOINT_PATH, &endpoint("air", "public", reps));
    commit(&dir, None, "publish air, reviewer note: SECRET-REVIEW-NOTE");
    write(
        &dir,
        ENDPOINT_PATH,
        &endpoint("air", "public", reps).replace("Ovzdušie", "Kvalita ovzdušia"),
    );
    commit(&dir, None, "title only");
    let reps = "  enabledRepresentations: [ngsi-ld, csv, ogc-features]\n";
    write(&dir, ENDPOINT_PATH, &endpoint("air", "public", reps));
    commit(&dir, None, "add ogc");
    git(&dir, None, &["checkout", "-q", "-b", "side"]);
    let hidden = format!("{reps}  projection:\n    hiddenAttributes: [pm25]\n");
    write(&dir, ENDPOINT_PATH, &endpoint("air", "public", &hidden));
    commit(&dir, None, "hide pm25 on a branch");
    git(&dir, None, &["checkout", "-q", "main"]);
    git(
        &dir,
        None,
        &["merge", "-q", "--no-ff", "-m", "merge side", "side"],
    );
    let deprecated = format!(
        "{hidden}  deprecation:\n    since: 2026-11-02\n    sunset: 2027-02-01\n    \
         link: https://example.org/v2\n    representations: [ogc-features]\n"
    );
    write(&dir, ENDPOINT_PATH, &endpoint("air", "public", &deprecated));
    commit(&dir, None, "deprecate ogc");
    write(
        &dir,
        ENDPOINT_PATH,
        &endpoint("air", "organization", &deprecated),
    );
    commit(&dir, None, "air goes internal");
    dir
}

#[test]
fn every_merged_public_change_in_the_window_is_listed_newest_first() {
    let dir = repository();
    let list = entries(&dir, Utc::now() - Duration::days(30)).expect("the history reads");
    let said: Vec<(&str, Vec<String>, Vec<Representation>)> = list
        .iter()
        .map(|e| (e.endpoint.as_str(), e.changes.clone(), e.surfaces.clone()))
        .collect();
    use Representation::{Csv, Mcp, NgsiLd, OgcFeatures};
    assert_eq!(
        said,
        vec![
            (
                "air",
                vec!["no longer public".to_owned()],
                vec![NgsiLd, Mcp, Csv, OgcFeatures]
            ),
            (
                "air",
                vec!["deprecated, sunset 2027-02-01; see https://example.org/v2".to_owned()],
                vec![OgcFeatures]
            ),
            (
                "air",
                vec!["attributes no longer served: pm25".to_owned()],
                vec![NgsiLd, Mcp, Csv, OgcFeatures]
            ),
            (
                "air",
                vec!["surface added: ogc-features".to_owned()],
                vec![OgcFeatures]
            ),
            ("air", vec!["published".to_owned()], vec![NgsiLd, Mcp, Csv]),
        ],
        "{list:#?}"
    );
    let first = &list[list.len() - 1];
    assert_eq!(first.address, "/api/endpoint/zt4qm7ge2xdv6ksb3ncf5arw2y/");
    assert_eq!(first.title.as_deref(), Some("Air <b>quality</b>"));
    assert_eq!(first.day, Utc::now().date_naive());
}

#[test]
fn the_page_groups_per_surface_and_carries_nothing_internal() {
    let dir = repository();
    let options = Options::parse(&["--repo-dir", dir.to_str().expect("utf-8")]).expect("parses");
    let page = options.render(Utc::now()).expect("renders");
    assert!(page.contains("last 30 days"), "{page}");
    for section in ["## ngsi-ld", "## csv", "## ogc-features", "## mcp"] {
        assert!(page.contains(section), "{section} missing:\n{page}");
    }
    assert!(page.contains("\\<b\\>quality\\</b\\>"), "{page}");
    for internal in [
        "SECRET-REVIEW-NOTE",
        "Reviewer Name",
        "r@example.org",
        "internal-air",
        "old-air",
        "ovzdusie",
        "<b>",
    ] {
        assert!(!page.contains(internal), "{internal} leaked:\n{page}");
    }

    let json = Options::parse(&[
        "--repo-dir",
        dir.to_str().expect("utf-8"),
        "--format",
        "json",
        "--days",
        "90",
    ])
    .expect("parses")
    .render(Utc::now())
    .expect("renders");
    let value: serde_json::Value = serde_json::from_str(&json).expect("json");
    let names: Vec<&str> = value
        .as_array()
        .expect("a list")
        .iter()
        .filter_map(|e| e["endpoint"].as_str())
        .collect();
    assert!(
        names.contains(&"old-air"),
        "90 days reach the old one: {names:?}"
    );
    assert!(!json.contains("SECRET-REVIEW-NOTE"));
}

#[test]
fn an_empty_window_says_so_and_bad_options_are_refused() {
    assert!(markdown(&[], 30).contains("No public Endpoint changed"));
    for bad in [
        &["--days", "30"][..],
        &["--repo-dir", "x", "--days", "0"],
        &["--repo-dir", "x", "--days", "367"],
        &["--repo-dir", "x", "--format", "html"],
        &["--repo-dir", "x", "--bogus", "1"],
        &["--repo-dir"],
    ] {
        assert_eq!(Options::parse(bad), None, "{bad:?}");
    }
    let missing = Options::parse(&["--repo-dir", "/nonexistent-changelog-dir"]).expect("parses");
    assert!(missing.render(Utc::now()).is_err());
}
