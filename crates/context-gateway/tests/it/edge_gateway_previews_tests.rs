//! Edge cases of `previews::Mirror::{apply, follow}` (T-1943, T-1944; CC-78, PF-83, PF-46,
//! AG-52, Architecture/06 §7.2).
//!
//! Contracts, one sentence each:
//!
//! - `apply`: the previews directory ends up holding exactly what the Portal listed, every file
//!   inside the preview's own directory, and nothing that is not a preview is touched.
//! - `follow`: a Portal that cannot be read, cannot be believed or cannot be reached changes
//!   nothing, so the previews last listed keep answering rather than disappearing.
//!
//! `preview_tests.rs` covers the happy path, the escaping file names and a preview that stops.
//! These are the shapes around them: a crashed run's leftovers, a directory that is not a
//! preview, the digest short circuit, and every way the poll can fail.

use crate::common;

use context_gateway::previews::{valid_prefix, valid_salt, Mirror, Preview, WorkloadToken};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn scratch(test: &str) -> PathBuf {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("a clock after 1970")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("gw-edge-preview-{test}-{now}"));
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    dir
}

fn files(of: &[(&str, &str)]) -> BTreeMap<String, String> {
    of.iter()
        .map(|(path, text)| ((*path).to_owned(), (*text).to_owned()))
        .collect()
}

fn one(prefix: &str, of: &[(&str, &str)]) -> Vec<Preview> {
    vec![Preview {
        prefix: prefix.to_owned(),
        files: files(of),
        ..Default::default()
    }]
}

/// The directory names under `dir`, sorted, including the ones `listed` hides.
fn names(dir: &Path) -> Vec<String> {
    let mut found: Vec<String> = std::fs::read_dir(dir)
        .expect("the directory")
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    found.sort();
    found
}

/// The files of the branch in a preview directory: [`names`] without the readers' file.
fn branch_names(dir: &Path) -> Vec<String> {
    let mut found = names(dir);
    found.retain(|name| name != "access.json");
    found
}

// --- apply (T-1943) ---------------------------------------------------------------------------

#[test]
fn the_bounds_of_a_prefix_are_the_bounds_and_not_one_character_more() {
    // The prefix is the isolation, and it is also a directory name: too short is not a name a
    // workspace could have minted, too long is a path the filesystem may refuse halfway.
    assert!(valid_prefix("ws-a-"), "five characters is the shortest");
    assert!(!valid_prefix("ws--"), "four is one too few");
    let longest = format!("ws-{}-", "a".repeat(60));
    assert_eq!(longest.len(), 64);
    assert!(valid_prefix(&longest));
    assert!(!valid_prefix(&format!("ws-{}-", "a".repeat(61))), "65");
    for odd in [
        "ws-á-",
        "ws-a_-",
        "ws-a -",
        "ws-a\n-",
        "WS-a-",
        "ws-a-\u{0}",
    ] {
        assert!(!valid_prefix(odd), "{odd:?} was read as a prefix");
    }
}

#[test]
fn a_preview_with_no_files_is_a_directory_and_not_a_missing_one() {
    // A branch can be emptied. The preview still exists, so the reaper has to find a directory
    // that renders to nothing rather than a name with nothing behind it.
    let dir = scratch("empty");
    Mirror::new(&dir)
        .apply(one("ws-air-", &[]))
        .expect("the empty preview is written");
    assert!(dir.join("ws-air-").is_dir());
    assert_eq!(names(&dir), vec!["ws-air-".to_owned()]);
}

#[test]
fn a_file_in_a_directory_that_does_not_exist_yet_brings_its_parents() {
    let dir = scratch("parents");
    Mirror::new(&dir)
        .apply(one(
            "ws-air-",
            &[("a/b/c/space.yaml", "kind: ContextSpace\n")],
        ))
        .expect("the nested file is written");
    assert_eq!(
        std::fs::read_to_string(dir.join("ws-air-/a/b/c/space.yaml")).expect("the file"),
        "kind: ContextSpace\n"
    );
}

#[test]
fn a_path_that_climbs_back_inside_is_still_refused() {
    // `sub/../space.yaml` ends inside the preview, and is refused anyway: the check is on the
    // components, not on where the path happens to land, because the second reading is the one
    // a symlink can change underneath.
    let dir = scratch("climb");
    Mirror::new(&dir)
        .apply(one(
            "ws-air-",
            &[
                ("sub/../space.yaml", "x"),
                ("", "x"),
                ("./hidden.yaml", "x"),
                ("kept.yaml", "y"),
            ],
        ))
        .expect("the rest is written");
    assert_eq!(
        branch_names(&dir.join("ws-air-")),
        vec!["kept.yaml".to_owned()]
    );
}

#[test]
fn what_a_crashed_run_left_staged_is_never_served() {
    // The staging directory is `.{prefix}`, which is not a prefix, so the reaper never loads it.
    // A run that died between writing and renaming leaves one behind, and the next apply has to
    // replace it rather than build on top of it.
    let dir = scratch("staging");
    std::fs::create_dir_all(dir.join(".ws-air-")).expect("the leftover staging directory");
    std::fs::write(dir.join(".ws-air-/ghost.yaml"), "from a dead run").expect("the ghost");

    Mirror::new(&dir)
        .apply(one("ws-air-", &[("space.yaml", "kind: ContextSpace\n")]))
        .expect("the preview is written");
    assert_eq!(
        branch_names(&dir.join("ws-air-")),
        vec!["space.yaml".to_owned()]
    );
    assert_eq!(names(&dir), vec!["ws-air-".to_owned()], "nothing is staged");
}

#[test]
fn a_preview_that_did_not_change_is_left_alone_and_one_that_vanished_is_written_again() {
    // The digest is what keeps a ten second poll from rewriting every preview forever. It must
    // not, however, be believed over the directory: a preview deleted underneath the gateway
    // would otherwise stay missing until its files changed.
    let dir = scratch("digest");
    let mut mirror = Mirror::new(&dir);
    let listed = || one("ws-air-", &[("space.yaml", "kind: ContextSpace\n")]);

    mirror.apply(listed()).expect("written");
    let written = std::fs::metadata(dir.join("ws-air-/space.yaml"))
        .and_then(|data| data.modified())
        .expect("a modification time");

    mirror.apply(listed()).expect("unchanged");
    assert_eq!(
        std::fs::metadata(dir.join("ws-air-/space.yaml"))
            .and_then(|data| data.modified())
            .expect("a modification time"),
        written,
        "an unchanged preview is not rewritten"
    );

    std::fs::remove_dir_all(dir.join("ws-air-")).expect("the preview is deleted underneath");
    mirror.apply(listed()).expect("written again");
    assert!(dir.join("ws-air-/space.yaml").is_file());
}

#[test]
fn a_change_of_content_alone_is_a_change() {
    // The digest is over the paths and the texts. A branch that edits one line without adding a
    // file has to reach the reaper, or a preview serves a manifest nobody can see any more.
    let dir = scratch("content");
    let mut mirror = Mirror::new(&dir);
    mirror
        .apply(one("ws-air-", &[("space.yaml", "name: air\n")]))
        .expect("written");
    mirror
        .apply(one("ws-air-", &[("space.yaml", "name: voda\n")]))
        .expect("rewritten");
    assert_eq!(
        std::fs::read_to_string(dir.join("ws-air-/space.yaml")).expect("the file"),
        "name: voda\n"
    );
}

#[test]
fn a_file_the_branch_deleted_is_gone_rather_than_left_behind() {
    // The preview is rewritten beside and swapped in, so a removal is a removal. Writing over the
    // directory in place would leave a manifest the branch no longer has, still being served.
    let dir = scratch("removed-file");
    let mut mirror = Mirror::new(&dir);
    mirror
        .apply(one("ws-air-", &[("a.yaml", "x"), ("b.yaml", "y")]))
        .expect("written");
    mirror
        .apply(one("ws-air-", &[("a.yaml", "x")]))
        .expect("rewritten");
    assert_eq!(
        branch_names(&dir.join("ws-air-")),
        vec!["a.yaml".to_owned()]
    );
}

#[test]
fn a_directory_that_is_not_a_preview_is_never_removed() {
    // `apply` owns the previews under its directory and nothing else. A deployment that mounted
    // the previews beside something of its own would otherwise lose it on the first poll.
    let dir = scratch("neighbour");
    std::fs::create_dir_all(dir.join("not-a-preview")).expect("a neighbour");
    std::fs::write(dir.join("not-a-preview/keep.yaml"), "x").expect("its file");
    std::fs::write(dir.join("loose.yaml"), "x").expect("a loose file");

    let mut mirror = Mirror::new(&dir);
    mirror
        .apply(one("ws-air-", &[("space.yaml", "x")]))
        .expect("written");
    mirror.apply(Vec::new()).expect("the preview stops");

    assert!(!dir.join("ws-air-").exists(), "the preview is gone");
    assert!(dir.join("not-a-preview/keep.yaml").is_file());
    assert!(dir.join("loose.yaml").is_file());
}

#[test]
fn a_preview_whose_prefix_is_no_prefix_writes_nothing_at_all() {
    // Not one of its files, not an empty directory: a name that is not `ws-{name}-` is a name
    // whose isolation nobody minted, so it is left out whole.
    let dir = scratch("bad-prefix");
    Mirror::new(&dir)
        .apply(vec![
            Preview {
                prefix: "../escape".to_owned(),
                files: files(&[("space.yaml", "x")]),
                ..Default::default()
            },
            Preview {
                prefix: "ws-Air-".to_owned(),
                files: files(&[("space.yaml", "x")]),
                ..Default::default()
            },
            Preview {
                prefix: "ws-air-".to_owned(),
                files: files(&[("space.yaml", "x")]),
                ..Default::default()
            },
        ])
        .expect("the good one is written");
    assert_eq!(names(&dir), vec!["ws-air-".to_owned()]);
}

#[test]
fn the_same_prefix_listed_twice_ends_as_the_last_one_and_not_as_both() {
    // The Portal should not list a workspace twice, and a directory holding half of each render
    // would be a preview nobody can explain. The last entry wins whole.
    let dir = scratch("duplicate");
    Mirror::new(&dir)
        .apply(vec![
            Preview {
                prefix: "ws-air-".to_owned(),
                files: files(&[("first.yaml", "1")]),
                ..Default::default()
            },
            Preview {
                prefix: "ws-air-".to_owned(),
                files: files(&[("second.yaml", "2")]),
                ..Default::default()
            },
        ])
        .expect("written");
    assert_eq!(
        branch_names(&dir.join("ws-air-")),
        vec!["second.yaml".to_owned()]
    );
}

#[test]
fn two_previews_never_see_each_others_files() {
    // The prefix is the isolation and the directory is what it is made of.
    let dir = scratch("two");
    Mirror::new(&dir)
        .apply(vec![
            Preview {
                prefix: "ws-air-".to_owned(),
                files: files(&[("air.yaml", "1")]),
                ..Default::default()
            },
            Preview {
                prefix: "ws-voda-".to_owned(),
                files: files(&[("voda.yaml", "2")]),
                ..Default::default()
            },
        ])
        .expect("written");
    assert_eq!(
        branch_names(&dir.join("ws-air-")),
        vec!["air.yaml".to_owned()]
    );
    assert_eq!(
        branch_names(&dir.join("ws-voda-")),
        vec!["voda.yaml".to_owned()]
    );
}

#[test]
fn a_file_is_written_exactly_as_the_branch_holds_it() {
    // A manifest is YAML, where a CR or a trailing space changes what parses. The mirror is a
    // copy, not a formatter.
    let dir = scratch("bytes");
    let text = "kind: ContextSpace\r\nname: vzduch–měřidlo \n\n";
    Mirror::new(&dir)
        .apply(one("ws-air-", &[("space.yaml", text)]))
        .expect("written");
    assert_eq!(
        std::fs::read_to_string(dir.join("ws-air-/space.yaml")).expect("the file"),
        text
    );
}

#[test]
fn a_previews_directory_that_does_not_exist_yet_is_made() {
    // The gateway starts before the Portal has listed anything, so the first apply is also the
    // one that creates the directory the reaper reads.
    let dir = scratch("missing").join("deeper/still");
    Mirror::new(&dir)
        .apply(one("ws-air-", &[("space.yaml", "x")]))
        .expect("the directory is made");
    assert!(dir.join("ws-air-/space.yaml").is_file());
}

// --- follow (T-1944) --------------------------------------------------------------------------

/// What one Portal was asked, and with what.
#[derive(Clone)]
struct Portal {
    /// The bodies to answer, in order; the last one is repeated.
    answers: Arc<Vec<(u16, String)>>,
    calls: Arc<AtomicUsize>,
    authorization: Arc<Mutex<Vec<String>>>,
}

/// A Portal whose internal listener answers `answers` in order, recording every call.
async fn portal(answers: Vec<(u16, String)>) -> (String, Portal) {
    let state = Portal {
        answers: Arc::new(answers),
        calls: Arc::new(AtomicUsize::new(0)),
        authorization: Arc::new(Mutex::new(Vec::new())),
    };
    let held = state.clone();
    let app = axum::Router::new().route(
        "/internal/previews",
        axum::routing::get(move |request: axum::extract::Request| {
            let state = held.clone();
            async move {
                let seen = state
                    .calls
                    .fetch_add(1, Ordering::SeqCst)
                    .min(state.answers.len() - 1);
                if let Ok(mut header) = state.authorization.lock() {
                    header.push(
                        request
                            .headers()
                            .get(axum::http::header::AUTHORIZATION)
                            .and_then(|value| value.to_str().ok())
                            .unwrap_or_default()
                            .to_owned(),
                    );
                }
                let (status, body) = state.answers[seen].clone();
                (
                    axum::http::StatusCode::from_u16(status).expect("a status"),
                    [(axum::http::header::CONTENT_TYPE, "application/json")],
                    body,
                )
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a free port");
    let address = listener.local_addr().expect("the bound address");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{address}/internal/previews"), state)
}

fn listing(prefix: &str, path: &str, text: &str) -> String {
    json!({ "items": [ { "prefix": prefix, "files": { path: text } } ] }).to_string()
}

/// Lets the paused clock run until `done` holds, so a test waits for polls rather than seconds.
async fn until(done: impl Fn() -> bool) {
    for _ in 0..600 {
        if done() {
            return;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    panic!("the poller never got there");
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn the_first_poll_writes_what_the_portal_lists_without_waiting_for_the_interval() {
    // A gateway that started while previews were running has to serve them now, not in ten
    // seconds: the first tick of the interval is immediate and this is what depends on it.
    let dir = scratch("follow-first");
    let (url, seen) = portal(vec![(200, listing("ws-air-", "space.yaml", "x"))]).await;
    tokio::spawn(Mirror::new(&dir).follow(url, None));

    until(|| seen.calls.load(Ordering::SeqCst) >= 1).await;
    until(|| dir.join("ws-air-/space.yaml").is_file()).await;
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_portal_that_stops_answering_leaves_the_previews_it_last_listed() {
    // This is the whole point of the loop's error handling: a Portal restart, a rollout or a
    // network blip must not take every preview down with it, because a preview disappearing is
    // a person's work disappearing mid-demonstration.
    let dir = scratch("follow-500");
    let (url, seen) = portal(vec![
        (200, listing("ws-air-", "space.yaml", "x")),
        (500, "the portal fell over".to_owned()),
    ])
    .await;
    tokio::spawn(Mirror::new(&dir).follow(url, None));

    until(|| dir.join("ws-air-/space.yaml").is_file()).await;
    until(|| seen.calls.load(Ordering::SeqCst) >= 3).await;
    assert!(
        dir.join("ws-air-/space.yaml").is_file(),
        "two failed polls removed a running preview"
    );
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_body_that_is_not_the_list_leaves_the_previews_alone() {
    // An answer that is not the list is a Portal speaking a shape this gateway does not know —
    // an upgrade, a proxy's error page, a login form. None of them says the previews stopped.
    let dir = scratch("follow-shape");
    let (url, seen) = portal(vec![
        (200, listing("ws-air-", "space.yaml", "x")),
        (200, "<html>not json at all</html>".to_owned()),
        (200, json!({ "items": "not a list" }).to_string()),
        (200, json!({ "items": [], "extra": true }).to_string()),
    ])
    .await;
    tokio::spawn(Mirror::new(&dir).follow(url, None));

    until(|| dir.join("ws-air-/space.yaml").is_file()).await;
    until(|| seen.calls.load(Ordering::SeqCst) >= 4).await;
    assert!(
        dir.join("ws-air-/space.yaml").is_file(),
        "a list that could not be read was taken for an empty one"
    );
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn an_empty_list_the_portal_really_sent_does_stop_the_previews() {
    // The other half of the contract: unreadable is not empty, and empty is not unreadable. A
    // workspace that was stopped has to stop answering.
    let dir = scratch("follow-empty");
    let (url, _) = portal(vec![
        (200, listing("ws-air-", "space.yaml", "x")),
        (200, json!({ "items": [] }).to_string()),
    ])
    .await;
    tokio::spawn(Mirror::new(&dir).follow(url, None));

    until(|| dir.join("ws-air-/space.yaml").is_file()).await;
    until(|| !dir.join("ws-air-").exists()).await;
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_portal_that_cannot_be_reached_at_all_is_not_an_empty_list_either() {
    // Nothing is listening on that port. The poll fails at the connection and the loop keeps
    // asking rather than ending, because the Portal comes back.
    let dir = scratch("follow-unreachable");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a free port");
    let closed = listener.local_addr().expect("the bound address");
    drop(listener);

    let mut mirror = Mirror::new(&dir);
    mirror
        .apply(one("ws-air-", &[("space.yaml", "x")]))
        .expect("a preview is already running");
    tokio::spawn(mirror.follow(format!("http://{closed}/internal/previews"), None));

    tokio::time::sleep(Duration::from_secs(45)).await;
    assert!(
        dir.join("ws-air-/space.yaml").is_file(),
        "an unreachable Portal took the previews down"
    );
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn the_gateway_names_itself_to_the_portal_on_every_poll() {
    // PF-46, AG-52: the Portal's internal listener answers a caller it can name. The token goes
    // on each request, not on the first one only, because it is refreshed under the loop.
    let dir = scratch("follow-token");
    let (token_url, _) = realm(300).await;
    let (url, seen) = portal(vec![(200, json!({ "items": [] }).to_string())]).await;
    let token = Arc::new(WorkloadToken::new(
        token_url,
        "context-gateway".to_owned(),
        "not-a-real-secret".to_owned(),
    ));
    tokio::spawn(Mirror::new(&dir).follow(url, Some(token)));

    until(|| seen.calls.load(Ordering::SeqCst) >= 2).await;
    let sent = seen.authorization.lock().expect("the header log").clone();
    assert!(sent.len() >= 2, "{sent:?}");
    for header in sent {
        assert!(header.starts_with("Bearer "), "{header:?}");
    }
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_gateway_that_cannot_mint_a_token_asks_nothing_rather_than_asking_anonymously() {
    // An anonymous poll would either be refused or, on a listener misconfigured to answer
    // anybody, read every workspace's files without a caller. The loop skips the round instead.
    let dir = scratch("follow-no-token");
    let (token_url, refused) = realm(0).await;
    let (url, seen) = portal(vec![(200, json!({ "items": [] }).to_string())]).await;
    let token = Arc::new(WorkloadToken::new(
        token_url,
        "context-gateway".to_owned(),
        "not-a-real-secret".to_owned(),
    ));
    tokio::spawn(Mirror::new(&dir).follow(url, Some(token)));

    until(|| refused.load(Ordering::SeqCst) >= 2).await;
    assert_eq!(
        seen.calls.load(Ordering::SeqCst),
        0,
        "the Portal was asked without a token"
    );
}

/// A realm that grants a token lasting `lifetime` seconds, or refuses when `lifetime` is zero,
/// counting what it was asked. `(token endpoint, refusals)`.
async fn realm(lifetime: u64) -> (String, Arc<AtomicUsize>) {
    let asked = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&asked);
    let app = axum::Router::new().route(
        "/token",
        axum::routing::post(move || {
            let asked = Arc::clone(&counted);
            async move {
                if lifetime == 0 {
                    asked.fetch_add(1, Ordering::SeqCst);
                    return (
                        axum::http::StatusCode::UNAUTHORIZED,
                        axum::Json(json!({ "error": "invalid_client" }) as Value),
                    );
                }
                (
                    axum::http::StatusCode::OK,
                    axum::Json(json!({
                        "access_token": "a-token",
                        "expires_in": lifetime,
                    }) as Value),
                )
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a free port");
    let address = listener.local_addr().expect("the bound address");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{address}/token"), asked)
}

// --- who reaches a preview (T-5569, CC-91..CC-93, AP-19, AP-50) --------------------------------

/// The origin's public Endpoint, which publishes its slug.
const PUBLIC_SLUG: &str = "zt4qm7ge2xdv6ksb3ncf5arw2y";
/// The salt the Portal minted for the preview.
const SALT: &str = "Q3vR8kM2pL9xT4nW7yB1cZ";
const AUTHOR: &str = "demo.author";
const REVIEWER: &str = "demo.reviewer";

/// The Keycloak user id (`sub`) of the person `user` names, what the Portal lists as a reader
/// (ADR-N-061).
fn id_of(user: &str) -> String {
    format!("f:1:{user}")
}

/// A branch of `helsinki` whose space `air` everyone may read and write through the public
/// Endpoint `public-air`.
fn public_branch() -> BTreeMap<String, String> {
    files(&[
        (
            "space.yaml",
            "apiVersion: joinedcontext.com/v1alpha1\nkind: ContextSpace\nmetadata:\n  name: air\n  namespace: helsinki\nspec:\n  isSandbox: false\n",
        ),
        (
            "endpoints/public-air.yaml",
            &format!("apiVersion: joinedcontext.com/v1alpha1\nkind: Endpoint\nmetadata:\n  name: public-air\n  namespace: helsinki\nspec:\n  contextSpaceRef: air\n  slug: {PUBLIC_SLUG}\n  audience: public\n  enabledRepresentations: [\"ngsi-ld\"]\n"),
        ),
        (
            "policies/everyone.yaml",
            "apiVersion: joinedcontext.com/v1alpha1\nkind: Policy\nmetadata:\n  name: everyone\n  namespace: helsinki\nspec:\n  contextSpaceRef: air\n  assigner: did:web:hel.fi\n  assignee: { kind: role, id: public }\n  operations: [queryEntity, createEntity]\n  information:\n    - entities:\n        - type: AirQualityObserved\n",
        ),
    ])
}

struct Served {
    realm: common::Realm,
    app: axum::Router,
    broker: common::BrokerStub,
    slug: String,
}

/// The gateway serving an empty `main` and the preview `ws-air-` of [`public_branch`], read by
/// its author and one reviewer.
async fn served(test: &str) -> Served {
    let main = scratch(&format!("{test}-main"));
    let accounts = main.join("projects/helsinki/access/serviceaccounts");
    std::fs::create_dir_all(&accounts).expect("an account directory");
    std::fs::write(
        accounts.join("sensor.yaml"),
        "apiVersion: joinedcontext.com/v1alpha1\nkind: ServiceAccount\nmetadata:\n  name: sensor\n  namespace: helsinki\nspec:\n  owner:\n    user: demo.steward\n  purpose: \"a workload of main\"\n  roles:\n    - role: member\n      scope: { project: helsinki }\n  credentials:\n    - kind: oauth-client\n      name: default\n",
    )
    .expect("the account");
    let previews = scratch(&format!("{test}-previews"));
    Mirror::new(&previews)
        .apply(vec![Preview {
            prefix: "ws-air-".to_owned(),
            files: public_branch(),
            readers: [AUTHOR, REVIEWER].map(id_of).into(),
            salt: SALT.to_owned(),
        }])
        .expect("the preview is written");
    let (endpoints, spaces, accounts, .., provenance) =
        context_gateway::store::load_with_previews(&main, None, Some(&previews))
            .expect("main and the preview load");
    let slug = jc_manifest::loader::preview_slug_salted("ws-air-", SALT, PUBLIC_SLUG);
    assert!(
        endpoints.iter().any(|e| e.slug == slug),
        "the preview serves"
    );
    let realm = common::Realm::new();
    let broker = common::BrokerStub::start(vec![json!([])]).await;
    let gateway = context_gateway::app::Gateway::new(
        context_gateway::proxy::Broker::new(broker.url.clone()),
        Box::new(context_gateway::pdp::PolicyPdp),
        "hel.fi",
    )
    .serve(endpoints)
    .authenticate(Arc::new(realm.verifier()), accounts, None);
    gateway.resolver.replace_spaces(spaces);
    gateway.replace_provenance(provenance);
    Served {
        realm,
        app: context_gateway::app::router(Arc::new(gateway)),
        broker,
        slug,
    }
}

fn person(realm: &common::Realm, user: &str) -> String {
    realm.mint(&json!({
        "iss": common::ISSUER,
        "sub": id_of(user),
        "aud": "context-gateway",
        "exp": common::in_seconds(300),
        "iat": common::in_seconds(-10),
        "preferred_username": user,
        "groups": ["/helsinki"],
    }))
}

/// `method` on `path` with `token`: the status and the body.
async fn call(
    served: &Served,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> (u16, Value) {
    use tower::ServiceExt;
    let mut request = axum::http::Request::builder().method(method).uri(path);
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    let body = match body {
        Some(body) => {
            request = request.header("content-type", "application/json");
            axum::body::Body::from(body.to_string())
        }
        None => axum::body::Body::empty(),
    };
    let response = served
        .app
        .clone()
        .oneshot(request.body(body).expect("a request"))
        .await
        .expect("the gateway answers");
    let status = response.status().as_u16();
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("a readable body");
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn entities(slug: &str) -> String {
    format!("/api/endpoint/{slug}/ngsi-ld/v1/entities?type=AirQualityObserved")
}

/// CC-91: a preview of a public Endpoint answers its author and its reviewer; the public, a
/// signed-in person who reviews nothing and the preview's space surface asked by either get the
/// 404 a slug nobody created gets, and the broker is never asked for them.
#[tokio::test]
async fn a_preview_of_a_public_endpoint_answers_its_author_and_reviewers_only() {
    let served = served("readers").await;
    let nobodys = call(
        &served,
        "GET",
        &entities("n0bodysq2mzt6vhx3nbwrs5cjd"),
        None,
        None,
    )
    .await;
    assert_eq!(nobodys.0, 404, "{nobodys:?}");

    let stranger = person(&served.realm, "demo.stranger");
    // A reader is the listed id: a token of another person that calls itself the author is not.
    let namesake = served.realm.mint(&json!({
        "iss": common::ISSUER,
        "sub": "f:1:demo.namesake",
        "aud": "context-gateway",
        "exp": common::in_seconds(300),
        "iat": common::in_seconds(-10),
        "preferred_username": AUTHOR,
        "groups": ["/helsinki"],
    }));
    for (who, token) in [
        ("anonymous", None),
        ("a stranger", Some(stranger.as_str())),
        ("the author's namesake", Some(namesake.as_str())),
    ] {
        for path in [
            entities(&served.slug),
            "/cs/ws-air-helsinki-air/ngsi-ld/v1/entities?type=AirQualityObserved".to_owned(),
        ] {
            let answer = call(&served, "GET", &path, token, None).await;
            assert_eq!(answer, nobodys, "{who} on {path}");
        }
    }
    assert!(
        served.broker.hops().is_empty(),
        "the broker was never asked"
    );

    for reader in [AUTHOR, REVIEWER] {
        let token = person(&served.realm, reader);
        let (status, body) =
            call(&served, "GET", &entities(&served.slug), Some(&token), None).await;
        assert_eq!(status, 200, "{reader}: {body}");
    }
    assert_eq!(
        served.broker.hops().len(),
        2,
        "both reads reached the broker"
    );
}

/// CC-91: a workload's token and a data space transfer token are never a reader, even when the
/// name they carry is one; a token the gateway refuses gets the same 404 as no token, so a
/// holder of the slug learns nothing about whether the preview exists.
#[tokio::test]
async fn a_workload_or_a_refused_token_naming_a_reader_gets_the_unknown_slug_404() {
    let served = served("workloads").await;
    let nobodys = call(
        &served,
        "GET",
        &entities("n0bodysq2mzt6vhx3nbwrs5cjd"),
        None,
        None,
    )
    .await;
    let claims = |extra: Value| {
        let mut claims = json!({
            "iss": common::ISSUER,
            "sub": id_of(AUTHOR),
            "aud": "context-gateway",
            "exp": common::in_seconds(300),
            "iat": common::in_seconds(-10),
            "preferred_username": AUTHOR,
        });
        claims
            .as_object_mut()
            .expect("an object")
            .extend(extra.as_object().expect("an object").clone());
        served.realm.mint(&claims)
    };
    let workload = claims(json!({
        "azp": context_gateway::auth::accounts::client_id("helsinki", "sensor"),
    }));
    let transfer = claims(json!({ "agreementId": "urn:ngsi-ld:Agreement:air-1" }));
    let expired =
        claims(json!({ "exp": common::in_seconds(-900), "iat": common::in_seconds(-1200) }));
    // A reader's own id under a client's service-account username (F-SEC1-031, T-5358).
    let named_as_a_client = claims(json!({
        "preferred_username": "service-account-portal-reconciler",
    }));
    for (who, token) in [
        ("a reader named as a client's account", named_as_a_client),
        ("a ServiceAccount", workload),
        ("a data space consumer", transfer),
        ("an expired token", expired),
        ("a token that is no token", "not.a.jwt".to_owned()),
    ] {
        let answer = call(&served, "GET", &entities(&served.slug), Some(&token), None).await;
        assert_eq!(answer, nobodys, "{who}");
    }
    assert!(
        served.broker.hops().is_empty(),
        "the broker was never asked"
    );
}

/// CC-92: the slug is not the digest of the prefix and the published slug, so the workspace
/// name and the origin's slug do not find it; the salt is the Portal's 128 bits and nothing else.
#[tokio::test]
async fn a_preview_slug_cannot_be_derived_from_public_inputs() {
    let served = served("salt").await;
    let guessed = jc_manifest::loader::preview_slug_salted("ws-air-", "", PUBLIC_SLUG);
    assert_ne!(served.slug, guessed);
    let author = person(&served.realm, AUTHOR);
    let (status, body) = call(&served, "GET", &entities(&guessed), Some(&author), None).await;
    assert_eq!(status, 404, "the unsalted slug serves nothing: {body}");
    assert_ne!(
        jc_manifest::loader::preview_slug_salted("ws-air-", "R3vR8kM2pL9xT4nW7yB1cZ", PUBLIC_SLUG),
        served.slug,
        "another salt, another slug"
    );
    assert!(valid_salt("") && valid_salt(SALT));
    for bad in [
        "short",
        "Q3vR8kM2pL9xT4nW7yB1c\n",
        &"a".repeat(129),
        "Q3vR8kM2pL9xT4nW7yB1c/",
    ] {
        assert!(!valid_salt(bad), "{bad:?}");
    }
    let dir = scratch("salt-refused");
    Mirror::new(&dir)
        .apply(vec![Preview {
            prefix: "ws-air-".to_owned(),
            files: public_branch(),
            salt: "Q3vR8kM2pL9xT4nW7yB1c\n".to_owned(),
            ..Default::default()
        }])
        .expect("the mirror runs");
    assert!(
        names(&dir).is_empty(),
        "a preview with a bad salt is left out"
    );
}

/// CC-93: a reader's write through a preview reaches the broker as theirs and is kept.
#[tokio::test]
async fn a_write_through_a_preview_is_attributed() {
    let served = served("write").await;
    let author = person(&served.realm, AUTHOR);
    let entity =
        json!({ "id": "urn:ngsi-ld:AirQualityObserved:ws-air-1", "type": "AirQualityObserved" });
    let path = format!("/api/endpoint/{}/ngsi-ld/v1/entities", served.slug);
    let (status, body) = call(&served, "POST", &path, None, Some(entity.clone())).await;
    assert_eq!(status, 404, "the public writes nothing: {body}");
    let (status, body) = call(&served, "POST", &path, Some(&author), Some(entity)).await;
    assert!(status < 300, "{status}: {body}");
    let hops = served.broker.hops();
    assert_eq!(hops.len(), 1, "{hops:?}");
    let headers = &hops[0].headers;
    let value = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    };
    assert_eq!(
        value("jc-provenance-actor").as_deref(),
        Some("user%3Ademo.author"),
        "{headers:?}"
    );
    assert_eq!(
        value("jc-provenance-audit").as_deref(),
        Some("changes"),
        "{headers:?}"
    );
}

/// CC-91: a branch that carries its own `access.json` does not name its readers: the Portal's
/// list does, and a mirror that cannot read the file reads nobody.
#[test]
fn a_branch_file_cannot_stand_in_for_the_readers() {
    let dir = scratch("forged-readers");
    Mirror::new(&dir)
        .apply(vec![Preview {
            prefix: "ws-air-".to_owned(),
            files: files(&[("access.json", r#"{"readers":["mallory"],"salt":""}"#)]),
            readers: [id_of(AUTHOR)].into(),
            ..Default::default()
        }])
        .expect("written");
    let access = context_gateway::previews::access_of(&dir.join("ws-air-"));
    assert_eq!(access.readers, [id_of(AUTHOR)].into());
    std::fs::write(dir.join("ws-air-/access.json"), "not json").expect("overwritten");
    assert!(context_gateway::previews::access_of(&dir.join("ws-air-"))
        .readers
        .is_empty());
}
