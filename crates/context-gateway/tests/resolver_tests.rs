use context_gateway::resolver::{Endpoint, SlugResolver};
use jc_core::kinds::{Audience, Representation};
use std::sync::Arc;

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
        creates: None,
        file_limits: None,
        hidden_attributes: Default::default(),
        projection: None,
        base_path: format!("/api/endpoint/{slug}"),
        models: Vec::new(),
        view_mapping: None,
        catalog: None,
        policy_names: Vec::new(),
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

/// EP-18: the lookup is on the hot path of every request, so it never waits for a writer.
/// Shown without a clock (T-3538): the writer is parked inside `replace`, its endpoints arriving
/// through a channel the test holds open, and every slug still resolves to the old table. A lock
/// spanning the build would leave the readers waiting until the writer is released, which the
/// test only does after they have all been answered.
#[test]
fn a_reader_is_answered_while_a_writer_is_inside_replace() {
    let resolver = Arc::new(SlugResolver::with(
        (0..500).map(|i| endpoint(&format!("slug{i:04}"), "before")),
    ));
    let (feed, endpoints) = std::sync::mpsc::channel::<Endpoint>();
    let (inside_tx, inside) = std::sync::mpsc::channel::<()>();
    let writer = {
        let resolver = Arc::clone(&resolver);
        std::thread::spawn(move || {
            resolver.replace(std::iter::from_fn(move || {
                let _ = inside_tx.send(());
                endpoints.recv().ok()
            }));
        })
    };
    inside.recv().expect("the writer is inside replace");

    let readers: Vec<_> = (0..4)
        .map(|_| {
            let resolver = Arc::clone(&resolver);
            std::thread::spawn(move || {
                (0..500)
                    .map(|i| {
                        resolver
                            .resolve(&format!("slug{i:04}"))
                            .map(|e| e.space.clone())
                    })
                    .collect::<Vec<_>>()
            })
        })
        .collect();
    for reader in readers {
        let spaces = reader.join().expect("the reader finishes");
        let stray = spaces
            .iter()
            .filter(|s| s.as_deref() != Some("before"))
            .count();
        assert_eq!(stray, 0, "{stray} of 500 lookups missed the old table");
    }

    for i in 0..500 {
        feed.send(endpoint(&format!("slug{i:04}"), "after"))
            .expect("the writer is still reading");
    }
    drop(feed);
    writer.join().expect("the writer finishes");
    assert_eq!(
        resolver.resolve("slug0499").expect("resolves").space,
        "after"
    );
}
