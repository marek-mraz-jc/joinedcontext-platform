//! The scale bench (T-4443, CC-54): one generated organization of N Endpoints
//! (`jcctl::bench_org`), measured offline. Run one size per process, so the peak RSS is that
//! size's:
//!
//! ```text
//! cargo run --release -p context-gateway --example scale_bench -- 10000
//! ```
//!
//! It prints one line of `key=value` wall times in milliseconds: the store load, the reaper's
//! idle tick (and the files it statted) and its tick after one project is relinked to a commit
//! that changes one Policy, a slug lookup in microseconds, and
//! `jcctl validate` over the tree; then the peak RSS in MiB. Not a CI gate.

use context_gateway::app::Gateway;
use context_gateway::pdp::reaper::Reaper;
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::store::{self, Checkouts};
use jcctl::assemble::Directories;
use std::sync::Arc;
use std::time::Instant;

fn ms(started: Instant) -> f64 {
    started.elapsed().as_secs_f64() * 1000.0
}

/// The process's peak resident set in MiB (`VmHWM`), 0 where /proc does not say.
fn peak_mib() -> f64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|status| {
            let line = status.lines().find(|l| l.starts_with("VmHWM:"))?;
            line.split_whitespace().nth(1)?.parse::<f64>().ok()
        })
        .map_or(0.0, |kib| kib / 1024.0)
}

fn main() {
    let n: usize = std::env::args()
        .nth(1)
        .and_then(|arg| arg.parse().ok())
        .unwrap_or(100);
    let root = std::env::temp_dir().join(format!("jc-scale-bench-{n}-{}", std::process::id()));
    let tree = jcctl::bench_org::write(&root, n).expect("the tree is written");
    // The shape `jcctl checkouts` leaves in production: each project a link to a tree named by
    // its commit under `.cache` (T-4444).
    let cache = tree.projects.join(".cache");
    std::fs::create_dir_all(&cache).expect("the cache");
    for project in std::fs::read_dir(&tree.projects)
        .expect("the checkouts")
        .flatten()
    {
        let name = project.file_name().into_string().expect("a name");
        if name.starts_with('.') {
            continue;
        }
        std::fs::rename(project.path(), cache.join(format!("{name}.tree-c1"))).expect("moved");
        std::os::unix::fs::symlink(format!(".cache/{name}.tree-c1"), project.path())
            .expect("linked");
    }
    let checkouts = Checkouts {
        projects: tree.projects.clone(),
        assembly: root.join("assembly"),
        renders: Default::default(),
    };

    let started = Instant::now();
    let (endpoints, spaces, ..) =
        store::load_with_previews(&tree.org, Some(&checkouts), None).expect("the tree loads");
    let load = ms(started);
    assert_eq!(endpoints.len(), n, "every Endpoint is served");

    let gateway = Arc::new(
        Gateway::new(
            Broker::new("http://127.0.0.1:1".to_owned()),
            Box::new(PolicyPdp),
            "bench.example",
        )
        .serve(endpoints)
        .serve_spaces(spaces),
    );
    let mut reaper = Reaper::new(Arc::clone(&gateway), &tree.org).with_checkouts(checkouts);
    // The first tick takes the fingerprint; the ones after it are what an idle second costs.
    reaper.tick();
    let mut idle = Vec::new();
    for _ in 0..5 {
        let started = Instant::now();
        assert!(!reaper.tick(), "nothing changed");
        idle.push(ms(started));
    }
    idle.sort_by(f64::total_cmp);

    let idle_files = reaper.files_walked();

    // The next commit of p0000 changes one Policy and is linked in by one rename.
    let next = cache.join("p0000.tree-c2");
    std::process::Command::new("cp")
        .arg("-r")
        .arg(cache.join("p0000.tree-c1"))
        .arg(&next)
        .status()
        .expect("the next commit");
    let policy = next.join("spaces/s00000/policies/r00000.yaml");
    let text = std::fs::read_to_string(&policy).expect("the policy");
    std::fs::write(
        &policy,
        text.replace("[retrieveOps]", "[retrieveOps, queryEntity]"),
    )
    .expect("the policy changes");
    let link = tree.projects.join(".p0000.link");
    std::os::unix::fs::symlink(".cache/p0000.tree-c2", &link).expect("the next link");
    std::fs::rename(&link, tree.projects.join("p0000")).expect("relinked");
    let started = Instant::now();
    assert!(reaper.tick(), "the policy changed");
    let changed = ms(started);

    let rounds = 10;
    let started = Instant::now();
    for _ in 0..rounds {
        for slug in &tree.slugs {
            assert!(gateway.resolver.resolve(slug).is_some());
        }
    }
    let resolve_us = started.elapsed().as_secs_f64() * 1e6 / (rounds * n) as f64;

    let directories = Directories(
        std::fs::read_dir(&tree.projects)
            .expect("the checkouts")
            .flatten()
            .filter_map(|e| Some((e.file_name().into_string().ok()?, e.path())))
            .filter(|(name, _)| !name.starts_with('.'))
            .collect(),
    );
    let started = Instant::now();
    let report = jcctl::commands::validate::run_assembled(&tree.org, &directories);
    let validate = ms(started);
    assert!(report.is_valid(), "{:?}", report.findings.first());

    println!(
        "endpoints={n} load_ms={load:.1} idle_tick_ms={:.3} idle_files={idle_files} policy_tick_ms={changed:.1} \
         resolve_us={resolve_us:.3} validate_ms={validate:.1} peak_mib={:.0}",
        idle[idle.len() / 2],
        peak_mib()
    );
    let _ = std::fs::remove_dir_all(&root);
}
