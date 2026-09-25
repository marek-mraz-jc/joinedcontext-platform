use context_gateway::resolver::{Endpoint, SlugResolver};
use jc_core::kinds::{Audience, Representation};
use std::sync::Arc;
use std::time::Instant;

const SLUG: &str = "zt4qm7ge2xdv6ksb3ncf5arw2y";

fn endpoint(slug: &str, space: &str) -> Endpoint {
    Endpoint {
        declared_types: None,
        roles: Default::default(),
        slug: slug.to_owned(),
        title: std::collections::BTreeMap::new(),
        description: std::collections::BTreeMap::new(),
        space: space.to_owned(),
        project: "ovzdusie".to_owned(),
        audience: Audience::Public,
        allowed_projects: Vec::new(),
        representations: vec![Representation::NgsiLd, Representation::GeoJson],
        rate_limit: None,
        file_limits: None,
        hidden_attributes: Default::default(),
        projection: None,
        base_path: format!("/api/endpoint/{slug}"),
        models: Vec::new(),
        view_mapping: None,
        policies: Vec::new(),
    }
}

#[test]
fn an_unknown_slug_resolves_to_nothing_at_all() {
    let resolver = SlugResolver::with([endpoint(SLUG, "ovzdusie")]);

    assert!(resolver.resolve(SLUG).is_some());
    // EP-03, EP-23: a guessed slug tells the caller nothing, not even that a space exists.
    assert!(resolver.resolve("abcdefghijklmnopqrstuvwxyz").is_none());
    assert!(resolver.resolve("").is_none());
    assert!(resolver.resolve(&SLUG.to_uppercase()).is_none());
}

#[test]
fn a_resolved_endpoint_carries_its_space_and_representations() {
    let resolver = SlugResolver::with([endpoint(SLUG, "ovzdusie")]);
    let resolved = resolver.resolve(SLUG).expect("the endpoint resolves");

    assert_eq!(resolved.space, "ovzdusie");
    assert!(resolved.serves(Representation::GeoJson));
    assert!(!resolved.serves(Representation::Csv));
}

/// EP-14, EP-15: who may use the endpoint at all, before any policy is evaluated.
#[test]
fn the_audience_decides_who_may_use_the_endpoint() {
    let public = endpoint(SLUG, "ovzdusie");
    assert!(public.admits(None));
    assert!(public.admits(Some("bb-doprava")));

    let organization = Endpoint {
        roles: Default::default(),
        audience: Audience::Organization,
        ..endpoint(SLUG, "ovzdusie")
    };
    assert!(
        !organization.admits(None),
        "anonymous is not the organization"
    );
    assert!(organization.admits(Some("bb-doprava")));

    let listed = Endpoint {
        roles: Default::default(),
        audience: Audience::ProjectList,
        allowed_projects: vec!["bb-doprava".to_owned()],
        ..endpoint(SLUG, "ovzdusie")
    };
    assert!(listed.admits(Some("bb-doprava")));
    assert!(
        listed.admits(Some("ovzdusie")),
        "the owning project always may"
    );
    assert!(!listed.admits(Some("bb-energie")));
    assert!(!listed.admits(None));
}

/// EP-19: the reconciler replaces the table; a reader sees the whole old one or the whole
/// new one, never a half-applied mixture.
#[test]
fn replacing_the_table_is_atomic_and_visible_at_once() {
    let resolver = SlugResolver::new();
    assert!(resolver.is_empty());

    let held: Arc<_> = resolver
        .resolve(SLUG)
        .unwrap_or_else(|| Arc::new(endpoint(SLUG, "before")));

    resolver.replace([endpoint(SLUG, "ovzdusie"), endpoint("second", "doprava")]);
    assert_eq!(resolver.len(), 2);
    assert_eq!(resolver.resolve(SLUG).expect("resolves").space, "ovzdusie");

    resolver.replace([endpoint(SLUG, "renamed")]);
    assert_eq!(resolver.len(), 1);
    assert_eq!(resolver.resolve(SLUG).expect("resolves").space, "renamed");
    assert!(
        resolver.resolve("second").is_none(),
        "the old table went whole"
    );

    // A snapshot taken before the swap keeps the values it had.
    assert_eq!(held.space, "before");
}

/// EP-18: the lookup is on the hot path of every request, so it has to stay far under
/// 100 microseconds even while the table is being replaced under the readers.
///
/// The reading is the 99th percentile, not the maximum: a debug-build test sharing a
/// container with other builds gets preempted, and a scheduler stall is not resolution
/// latency. The percentile still fails loudly if the lookup itself ever starts blocking.
#[test]
fn resolution_stays_under_a_hundred_microseconds_under_concurrent_swaps() {
    let resolver = Arc::new(SlugResolver::with(
        (0..500).map(|i| endpoint(&format!("slug{i:04}"), "ovzdusie")),
    ));
    let slugs: Arc<Vec<String>> = Arc::new((0..500).map(|i| format!("slug{i:04}")).collect());

    let writer = {
        let resolver = Arc::clone(&resolver);
        std::thread::spawn(move || {
            for round in 0..50 {
                resolver.replace(
                    (0..500).map(|i| endpoint(&format!("slug{i:04}"), &format!("space{round}"))),
                );
            }
        })
    };

    let readers: Vec<_> = (0..4)
        .map(|_| {
            let resolver = Arc::clone(&resolver);
            let slugs = Arc::clone(&slugs);
            std::thread::spawn(move || {
                let mut timings = Vec::with_capacity(20_000);
                for i in 0..20_000 {
                    let slug = &slugs[i % slugs.len()];
                    let started = Instant::now();
                    let resolved = resolver.resolve(slug);
                    timings.push(started.elapsed());
                    assert!(resolved.is_some(), "{slug} is in every table");
                }
                timings.sort_unstable();
                timings
            })
        })
        .collect();

    writer.join().expect("the writer finishes");
    for reader in readers {
        let timings = reader.join().expect("the reader finishes");
        let p99 = timings[timings.len() * 99 / 100];
        let median = timings[timings.len() / 2];
        assert!(
            p99 < std::time::Duration::from_micros(100),
            "99th percentile was {p99:?}, median {median:?}"
        );
    }
}
