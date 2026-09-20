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

fn files() -> BTreeMap<String, String> {
    BTreeMap::from([
        (
            "space.yaml".to_owned(),
            "apiVersion: joinedcontext.com/v1alpha1\nkind: ContextSpace\nmetadata:\n  name: air\n  namespace: helsinki\nspec:\n  isSandbox: false\n".to_owned(),
        ),
        (
            "endpoints/public-air.yaml".to_owned(),
            format!("apiVersion: joinedcontext.com/v1alpha1\nkind: Endpoint\nmetadata:\n  name: public-air\n  namespace: helsinki\nspec:\n  contextSpaceRef: air\n  slug: {ORIGIN_SLUG}\n  audience: public\n  enabledRepresentations: [\"ngsi-ld\"]\n"),
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

/// `main` holds the same space and endpoint the preview was branched from.
fn main_repo(test: &str) -> PathBuf {
    let dir = scratch(&format!("{test}-main"));
    for (path, text) in files() {
        let path = dir.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
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

    let (endpoints, spaces, ..) = store::load_with_previews(&main, Some(&previews)).unwrap();
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
    let (endpoints, ..) = store::load_with_previews(&main, Some(&previews)).unwrap();
    assert_eq!(endpoints.len(), 1);
    assert_eq!(endpoints[0].slug, ORIGIN_SLUG);
}
