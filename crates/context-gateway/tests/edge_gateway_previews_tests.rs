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

use context_gateway::previews::{valid_prefix, Mirror, Preview, WorkloadToken};
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
    assert_eq!(names(&dir.join("ws-air-")), vec!["kept.yaml".to_owned()]);
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
    assert_eq!(names(&dir.join("ws-air-")), vec!["space.yaml".to_owned()]);
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
    assert_eq!(names(&dir.join("ws-air-")), vec!["a.yaml".to_owned()]);
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
            },
            Preview {
                prefix: "ws-Air-".to_owned(),
                files: files(&[("space.yaml", "x")]),
            },
            Preview {
                prefix: "ws-air-".to_owned(),
                files: files(&[("space.yaml", "x")]),
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
            },
            Preview {
                prefix: "ws-air-".to_owned(),
                files: files(&[("second.yaml", "2")]),
            },
        ])
        .expect("written");
    assert_eq!(names(&dir.join("ws-air-")), vec!["second.yaml".to_owned()]);
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
            },
            Preview {
                prefix: "ws-voda-".to_owned(),
                files: files(&[("voda.yaml", "2")]),
            },
        ])
        .expect("written");
    assert_eq!(names(&dir.join("ws-air-")), vec!["air.yaml".to_owned()]);
    assert_eq!(names(&dir.join("ws-voda-")), vec!["voda.yaml".to_owned()]);
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
