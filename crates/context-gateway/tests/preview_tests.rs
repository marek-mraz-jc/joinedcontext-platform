//! Workspace previews served beside `main` (CC-78, PF-83, Architecture/06 §7.2).

use context_gateway::app::Gateway;
use context_gateway::pdp::reaper::Reaper;
use context_gateway::pdp::PolicyPdp;
use context_gateway::previews::{valid_prefix, Mirror, Preview};
use context_gateway::proxy::Broker;
use context_gateway::store;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

const ORIGIN_SLUG: &str = "zt4qm7ge2xdv6ksb3ncf5arw2y";

/// The space manifest, with the `{space}` segment pinned when `pin` names one (PF-84).
fn space(pin: Option<&str>) -> String {
    let pinned = pin
        .map(|pin| format!("  urnSegment: {pin}\n"))
        .unwrap_or_default();
    format!("apiVersion: joinedcontext.com/v1alpha1\nkind: ContextSpace\nmetadata:\n  name: air\n  namespace: helsinki\nspec:\n  isSandbox: false\n{pinned}")
}

/// The endpoint manifest on `slug`.
fn endpoint(slug: &str) -> String {
    format!("apiVersion: joinedcontext.com/v1alpha1\nkind: Endpoint\nmetadata:\n  name: public-air\n  namespace: helsinki\nspec:\n  contextSpaceRef: air\n  slug: {slug}\n  audience: public\n  enabledRepresentations: [\"ngsi-ld\"]\n")
}

fn files() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("space.yaml".to_owned(), space(None)),
        (
            "endpoints/public-air.yaml".to_owned(),
            endpoint(ORIGIN_SLUG),
        ),
    ])
}

fn scratch(test: &str) -> PathBuf {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("gw-preview-{test}-{now}"));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A repository directory holding `of`.
fn repo_of(test: &str, of: BTreeMap<String, String>) -> PathBuf {
    let dir = scratch(test);
    for (path, text) in of {
        let path = dir.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    dir
}

/// `main` holds the same space and endpoint the preview was branched from.
fn main_repo(test: &str) -> PathBuf {
    repo_of(&format!("{test}-main"), files())
}

/// The preview of `ws-air-`, written under a previews directory of its own.
fn previews_of(test: &str, of: BTreeMap<String, String>) -> PathBuf {
    let dir = scratch(test);
    Mirror::new(&dir)
        .apply(vec![Preview {
            prefix: "ws-air-".to_owned(),
            files: of,
        }])
        .unwrap();
    dir
}

fn gateway() -> Arc<Gateway> {
    Arc::new(Gateway::new(
        Broker::new("http://127.0.0.1:1".to_owned()),
        Box::new(PolicyPdp),
        "hel.fi",
    ))
}

#[test]
fn only_a_workspace_prefix_is_a_prefix() {
    assert!(valid_prefix("ws-air-"));
    for bad in [
        "",
        "ws-",
        "air-",
        "ws-air",
        "ws-Air-",
        "ws-../-",
        "ws-a/b-",
        &format!("ws-{}-", "a".repeat(70)),
    ] {
        assert!(!valid_prefix(bad), "{bad}");
    }
}

#[test]
fn a_running_preview_answers_on_its_own_slug_beside_main() {
    let main = main_repo("serve");
    let previews = scratch("serve-previews");
    let mut mirror = Mirror::new(&previews);
    mirror
        .apply(vec![Preview {
            prefix: "ws-air-".to_owned(),
            files: files(),
        }])
        .unwrap();

    let (endpoints, spaces, ..) = store::load_with_previews(&main, None, Some(&previews)).unwrap();
    let minted = jcctl::loader::preview_slug("ws-air-", ORIGIN_SLUG);
    let origin = endpoints
        .iter()
        .find(|e| e.slug == ORIGIN_SLUG)
        .expect("main still answers");
    assert_eq!(origin.space, "helsinki-air");
    let preview = endpoints
        .iter()
        .find(|e| e.slug == minted)
        .expect("the preview answers");
    assert_eq!(
        preview.space, "ws-air-helsinki-air",
        "its own tenant, never the origin's"
    );
    assert_eq!(preview.project, "ws-air-helsinki");
    assert!(spaces
        .iter()
        .any(|s| s.endpoint.space == "ws-air-helsinki-air"));
    assert_eq!(endpoints.len(), 2);
}

#[test]
fn the_reaper_picks_up_a_preview_and_drops_it_when_it_stops() {
    let main = main_repo("reaper");
    let previews = scratch("reaper-previews");
    let gateway = gateway();
    let mut reaper = Reaper::new(Arc::clone(&gateway), &main).with_previews(&previews);
    let minted = jcctl::loader::preview_slug("ws-air-", ORIGIN_SLUG);
    let mut mirror = Mirror::new(&previews);

    mirror
        .apply(vec![Preview {
            prefix: "ws-air-".to_owned(),
            files: files(),
        }])
        .unwrap();
    assert!(reaper.tick(), "a new preview is a change");
    assert!(gateway.resolver.resolve(&minted).is_some());

    mirror.apply(Vec::new()).unwrap();
    assert!(!previews.join("ws-air-").exists());
    assert!(reaper.tick());
    assert!(
        gateway.resolver.resolve(&minted).is_none(),
        "a stopped preview answers 404"
    );
    assert!(
        gateway.resolver.resolve(ORIGIN_SLUG).is_some(),
        "main is untouched"
    );
}

#[test]
fn a_preview_never_writes_outside_its_directory_and_a_bad_one_is_left_out() {
    let previews = scratch("escape");
    let mut mirror = Mirror::new(previews.join("inner"));
    let mut hostile = files();
    hostile.insert("../../escaped.yaml".to_owned(), "x".to_owned());
    hostile.insert("/etc/escaped.yaml".to_owned(), "x".to_owned());
    hostile.insert(".git/config".to_owned(), "x".to_owned());
    mirror
        .apply(vec![
            Preview {
                prefix: "ws-air-".to_owned(),
                files: hostile,
            },
            Preview {
                prefix: "../".to_owned(),
                files: files(),
            },
        ])
        .unwrap();
    assert!(!previews.join("escaped.yaml").exists());
    assert!(!previews.join("inner/ws-air-/.git").exists());
    assert!(previews.join("inner/ws-air-/space.yaml").exists());
    let names: Vec<_> = std::fs::read_dir(previews.join("inner"))
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, vec!["ws-air-".to_owned()]);
}

#[test]
fn a_preview_that_does_not_render_leaves_main_serving() {
    let main = main_repo("broken");
    let previews = scratch("broken-previews");
    let mut broken = files();
    broken.insert(
        "endpoints/public-air.yaml".to_owned(),
        "kind: [not a manifest".to_owned(),
    );
    Mirror::new(&previews)
        .apply(vec![Preview {
            prefix: "ws-air-".to_owned(),
            files: broken,
        }])
        .unwrap();
    let (endpoints, ..) = store::load_with_previews(&main, None, Some(&previews)).unwrap();
    assert_eq!(endpoints.len(), 1);
    assert_eq!(endpoints[0].slug, ORIGIN_SLUG);
}

// -------------------------------------------------------------------------------------------------
// T-1709 "a copy or a preview as a way around review": the two collisions the minting alone does
// not rule out. A slug in a manifest is any 26 base32 characters somebody wrote, and PF-84 lets a
// space pin its `{space}` segment, so both names a preview renders can already be taken. The rule
// the gateway holds is that main wins: a preview answers beside main, never in its place (PF-83).
// -------------------------------------------------------------------------------------------------

/// PF-83: `main` already serves the slug this preview renders, so the preview's Endpoint is left
/// out and the caller of that slug keeps reaching main's tenant.
#[test]
fn a_preview_never_takes_over_a_slug_main_already_serves() {
    let minted = jcctl::loader::preview_slug("ws-air-", ORIGIN_SLUG);
    let mut theirs = files();
    theirs.insert("endpoints/public-air.yaml".to_owned(), endpoint(&minted));
    let main = repo_of("slug-taken-main", theirs);
    let previews = previews_of("slug-taken-previews", files());

    let (endpoints, ..) = store::load_with_previews(&main, None, Some(&previews)).unwrap();
    let on_the_slug: Vec<_> = endpoints.iter().filter(|e| e.slug == minted).collect();
    assert_eq!(
        on_the_slug.len(),
        1,
        "two records answer one slug: {endpoints:?}"
    );
    assert_eq!(
        on_the_slug[0].space, "helsinki-air",
        "the slug still names main's tenant, not the preview's"
    );
    assert_eq!(on_the_slug[0].project, "helsinki");
}

/// PF-83: `main` pins the very segment this preview renders, so neither the preview's space nor
/// any Endpoint of it answers on main's tenant — a preview that could would read and write the
/// data of the project it was copied from.
#[test]
fn a_preview_never_answers_on_a_space_main_already_serves() {
    const TAKEN: &str = "ws-air-helsinki-air";
    let mut theirs = files();
    theirs.insert("space.yaml".to_owned(), space(Some(TAKEN)));
    let main = repo_of("space-taken-main", theirs);
    let previews = previews_of("space-taken-previews", files());

    let (endpoints, spaces, ..) = store::load_with_previews(&main, None, Some(&previews)).unwrap();
    let on_the_space: Vec<_> = spaces.iter().filter(|s| s.name() == TAKEN).collect();
    assert_eq!(on_the_space.len(), 1, "two spaces on one segment");
    assert_eq!(
        on_the_space[0].endpoint.project, "helsinki",
        "the space is main's, not the preview's"
    );
    let taken_over: Vec<_> = endpoints
        .iter()
        .filter(|e| e.space == TAKEN && e.slug != ORIGIN_SLUG)
        .collect();
    assert!(
        taken_over.is_empty(),
        "a preview Endpoint answers on main's tenant: {taken_over:?}"
    );
}
