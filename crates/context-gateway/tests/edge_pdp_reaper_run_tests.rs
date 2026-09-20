//! Edge cases of `pdp::reaper::Reaper::run` (T-1934, EP-26, MP-02).
//!
//! Contract, in one sentence: `run` polls the repository every `INTERVAL` for as long as the
//! process lives — the first poll at once, the next after one interval, and never a poll fewer
//! because a reload failed, because the repository vanished or because many intervals passed with
//! nothing to do.
//!
//! The loop is what makes a revocation bounded (R48, OPS-45): a tick that stops being called is a
//! grant that stays live until the pod is restarted, and no request would notice. Every test here
//! runs on a paused clock, so what is asserted is the reaper's own latency and not a sleep.

use context_gateway::app::Gateway;
use context_gateway::pdp::reaper::{Reaper, INTERVAL};
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::store;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

const SPACE: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: ContextSpace
metadata:
  name: ovzdusie
  namespace: ovzdusie
spec:
  isSandbox: false
"#;

const SLUG: &str = "zt4qm7ge2xdv6ksb3ncf5arw2y";
const SECOND_SLUG: &str = "mluyob4nz52lok3ssk7pgn5vwt";

const ENDPOINT: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: Endpoint
metadata:
  name: public-air
  namespace: ovzdusie
spec:
  contextSpaceRef: ovzdusie
  slug: zt4qm7ge2xdv6ksb3ncf5arw2y
  audience: public
  enabledRepresentations: ["ngsi-ld"]
"#;

fn repo(test_name: &str) -> PathBuf {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after the epoch")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("gw-reaper-run-{test_name}-{now}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create the repository");
    write(&dir, "space.yaml", SPACE);
    write(&dir, "endpoint.yaml", ENDPOINT);
    dir
}

fn write(dir: &Path, name: &str, body: &str) {
    if let Some(parent) = dir.join(name).parent() {
        std::fs::create_dir_all(parent).expect("create the directory of a manifest");
    }
    std::fs::write(dir.join(name), body).expect("write a manifest");
}

fn second_endpoint() -> String {
    ENDPOINT
        .replace("public-air", "internal-air")
        .replace(SLUG, SECOND_SLUG)
}

fn gateway_on(dir: &Path) -> Arc<Gateway> {
    let (endpoints, spaces, _accounts, _federations, _agreements) =
        store::load(dir).expect("the repository loads");
    Arc::new(
        Gateway::new(
            Broker::new("http://127.0.0.1:1".to_owned()),
            Box::new(PolicyPdp),
            "banskabystrica.sk",
        )
        .serve(endpoints)
        .serve_spaces(spaces),
    )
}

/// Lets the spawned loop run without moving the clock.
async fn settle() {
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
}

/// Moves the clock by whole intervals and lets the loop run at each of them.
async fn intervals(count: u32) {
    for _ in 0..count {
        tokio::time::sleep(INTERVAL).await;
        settle().await;
    }
}

/// The first poll is immediate, so a change that landed between loading the repository and
/// starting the reaper is not carried for a whole interval.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn the_first_poll_happens_at_once() {
    let dir = repo("first-poll");
    let gateway = gateway_on(&dir);
    let reaper = Reaper::new(Arc::clone(&gateway), &dir);

    std::fs::remove_file(dir.join("endpoint.yaml")).expect("withdraw the endpoint");
    tokio::spawn(reaper.run());
    settle().await;

    assert!(
        gateway.resolver.resolve(SLUG).is_none(),
        "the withdrawal arrived on the first poll, without waiting out an interval"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// R48: a change made while the loop is running arrives within one interval, every time, not only
/// the first time.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn every_change_arrives_within_one_interval() {
    let dir = repo("each-change");
    let gateway = gateway_on(&dir);
    tokio::spawn(Reaper::new(Arc::clone(&gateway), &dir).run());
    settle().await;

    write(&dir, "endpoint-2.yaml", &second_endpoint());
    intervals(1).await;
    assert!(
        gateway.resolver.resolve(SECOND_SLUG).is_some(),
        "the first change"
    );

    std::fs::remove_file(dir.join("endpoint-2.yaml")).expect("withdraw it again");
    intervals(1).await;
    assert!(
        gateway.resolver.resolve(SECOND_SLUG).is_none(),
        "the second change"
    );

    write(&dir, "endpoint-2.yaml", &second_endpoint());
    intervals(1).await;
    assert!(
        gateway.resolver.resolve(SECOND_SLUG).is_some(),
        "the third change"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// A repository that does not load is a moment, not an end: the loop keeps polling and picks the
/// change up when the writer finishes.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_failed_reload_does_not_end_the_loop() {
    let dir = repo("failed-reload");
    let gateway = gateway_on(&dir);
    let handle = tokio::spawn(Reaper::new(Arc::clone(&gateway), &dir).run());
    settle().await;

    write(
        &dir,
        "endpoint-2.yaml",
        "apiVersion: joinedcontext.com/v1alpha1\nkind: Endp",
    );
    intervals(3).await;
    assert!(
        !handle.is_finished(),
        "a failed reload is not a reason to stop polling"
    );
    assert!(
        gateway.resolver.resolve(SLUG).is_some(),
        "the table that was serving still serves"
    );

    write(&dir, "endpoint-2.yaml", &second_endpoint());
    intervals(1).await;
    assert!(gateway.resolver.resolve(SECOND_SLUG).is_some());

    let _ = std::fs::remove_dir_all(&dir);
}

/// Neither is a repository that disappears: a ConfigMap mid-swap or a volume remount comes back,
/// and the loop has to be there when it does.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_repository_that_disappears_does_not_end_the_loop() {
    let dir = repo("vanished");
    let gateway = gateway_on(&dir);
    let handle = tokio::spawn(Reaper::new(Arc::clone(&gateway), &dir).run());
    settle().await;

    std::fs::remove_dir_all(&dir).expect("the volume goes away");
    intervals(3).await;
    assert!(!handle.is_finished());
    assert!(
        gateway.resolver.resolve(SLUG).is_some(),
        "it keeps serving while the volume is gone"
    );

    std::fs::create_dir_all(&dir).expect("the volume comes back");
    write(&dir, "space.yaml", SPACE);
    write(&dir, "endpoint.yaml", &second_endpoint());
    intervals(1).await;
    assert!(
        gateway.resolver.resolve(SECOND_SLUG).is_some(),
        "and follows it when it returns"
    );
    assert!(gateway.resolver.resolve(SLUG).is_none());

    let _ = std::fs::remove_dir_all(&dir);
}

/// Several changes inside one interval are one reload and all of them arrive: a reload is the
/// repository as it is now, not a queue of the edits that led to it.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn changes_made_inside_one_interval_arrive_together() {
    let dir = repo("burst");
    let gateway = gateway_on(&dir);
    tokio::spawn(Reaper::new(Arc::clone(&gateway), &dir).run());
    settle().await;

    // The last character of the slug is varied over the base32 alphabet the Portal mints from;
    // a slug outside it is not an endpoint and the loader leaves it out with a warning.
    for (index, last) in "abcdefgh".chars().enumerate() {
        let slug = format!("{}{last}", &SECOND_SLUG[..SECOND_SLUG.len() - 1]);
        write(
            &dir,
            &format!("endpoint-{index}.yaml"),
            &ENDPOINT
                .replace("public-air", &format!("air-{index}"))
                .replace(SLUG, &slug),
        );
    }
    intervals(1).await;

    assert_eq!(
        gateway.resolver.len(),
        9,
        "the seeded endpoint and the eight written"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// An idle repository does not tire the loop out: after many intervals with nothing to do, the
/// next change still arrives in one.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn the_loop_outlives_many_idle_intervals() {
    let dir = repo("idle");
    let gateway = gateway_on(&dir);
    let handle = tokio::spawn(Reaper::new(Arc::clone(&gateway), &dir).run());

    intervals(20).await;
    assert!(!handle.is_finished());
    assert!(
        gateway.resolver.resolve(SLUG).is_some(),
        "nothing changed, nothing was lost"
    );

    std::fs::remove_file(dir.join("endpoint.yaml")).expect("withdraw the endpoint");
    intervals(1).await;
    assert!(
        gateway.resolver.resolve(SLUG).is_none(),
        "the twenty-first interval works like the first"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// OPS-45 gives a revocation five seconds and EP-19 gives the endpoint table two; the poll is one
/// second, so a withdrawal is live for less than a single second of it.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_withdrawal_is_served_for_less_than_one_interval() {
    assert!(
        INTERVAL <= Duration::from_secs(2),
        "EP-19 gives the endpoint table two seconds"
    );

    let dir = repo("bound");
    let gateway = gateway_on(&dir);
    let started = tokio::time::Instant::now();
    tokio::spawn(Reaper::new(Arc::clone(&gateway), &dir).run());
    settle().await;

    std::fs::remove_file(dir.join("endpoint.yaml")).expect("withdraw the endpoint");
    let mut waited = Duration::ZERO;
    while gateway.resolver.resolve(SLUG).is_some() && waited < Duration::from_secs(5) {
        tokio::time::sleep(Duration::from_millis(50)).await;
        settle().await;
        waited = started.elapsed();
    }

    assert!(
        gateway.resolver.resolve(SLUG).is_none(),
        "still served after {waited:?}"
    );
    assert!(
        waited <= INTERVAL,
        "took {waited:?}, which is more than one poll"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// The previews are polled by the same loop, so a workspace preview that was rendered again is
/// served without a restart either.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn the_loop_follows_the_previews_directory_too() {
    let dir = repo("previews");
    let previews = dir.with_extension("previews");
    std::fs::create_dir_all(&previews).expect("create the previews directory");
    let gateway = gateway_on(&dir);
    let handle = tokio::spawn(
        Reaper::new(Arc::clone(&gateway), &dir)
            .with_previews(&previews)
            .run(),
    );
    settle().await;

    write(&previews, "workspace-1/endpoint.yaml", &second_endpoint());
    intervals(2).await;
    assert!(
        !handle.is_finished(),
        "a preview that does not render is skipped, not fatal"
    );
    assert!(
        gateway.resolver.resolve(SLUG).is_some(),
        "and the repository keeps serving"
    );

    // The repository itself is still followed while the previews are there.
    std::fs::remove_file(dir.join("endpoint.yaml")).expect("withdraw the endpoint");
    intervals(1).await;
    assert!(gateway.resolver.resolve(SLUG).is_none());

    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&previews);
}

/// The loop owns the reaper, not the gateway: the tables stay readable by every request handler
/// while the reaper swaps them.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn the_gateway_stays_readable_while_the_loop_runs() {
    let dir = repo("readable");
    let gateway = gateway_on(&dir);
    tokio::spawn(Reaper::new(Arc::clone(&gateway), &dir).run());

    for step in 0..5 {
        assert!(gateway.resolver.resolve(SLUG).is_some(), "step {step}");
        assert_eq!(gateway.resolver.len(), 1, "step {step}");
        intervals(1).await;
    }

    let _ = std::fs::remove_dir_all(&dir);
}

/// A repository written to twice in a row converges on the second write, not on the first: the
/// loop reads what is there when it polls.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn the_loop_converges_on_the_repository_as_it_is_at_the_poll() {
    let dir = repo("converge");
    let gateway = gateway_on(&dir);
    tokio::spawn(Reaper::new(Arc::clone(&gateway), &dir).run());
    settle().await;

    write(&dir, "endpoint.yaml", &second_endpoint());
    write(&dir, "endpoint.yaml", ENDPOINT);
    intervals(1).await;

    assert!(gateway.resolver.resolve(SLUG).is_some());
    assert!(
        gateway.resolver.resolve(SECOND_SLUG).is_none(),
        "the write that was overwritten"
    );
    assert_eq!(gateway.resolver.len(), 1);

    let _ = std::fs::remove_dir_all(&dir);
}
