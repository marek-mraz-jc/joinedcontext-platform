//! T-2860: the Endpoints an App reads, one rule for the Portal and the Context Gateway (AP-04,
//! AP-113, Architecture/16 section 12).

use jc_core::kinds::app::{
    served_endpoints, EndpointFact, ReferenceFact, APP_ENDPOINT_GENERATOR, MAX_APP_ENDPOINTS,
};
use jc_core::kinds::App;

fn app(needs: &[&str]) -> App {
    let needs: String = needs
        .iter()
        .map(|space| {
            format!(
                "    - contextSpaceRef: {{ kind: ContextSpace, name: {space} }}\n      types: [Alert]\n      operations: [queryEntity]\n"
            )
        })
        .collect();
    App::from_yaml(&format!(
        "apiVersion: joinedcontext.com/v1alpha1\nkind: App\nmetadata:\n  name: alerts\n  namespace: helsinki\nspec:\n  kind: static\n  source: {{ path: ./src }}\n  build: {{}}\n  visibility: organization\n  dataNeeds:\n{needs}"
    ))
    .expect("a valid App")
}

fn endpoint(project: &str, name: &str, slug: &str, space: &str) -> EndpointFact {
    EndpointFact {
        project: project.to_owned(),
        name: name.to_owned(),
        slug: slug.to_owned(),
        space: space.to_owned(),
        generated_by: None,
        public: false,
    }
}

fn public(mut endpoint: EndpointFact) -> EndpointFact {
    endpoint.public = true;
    endpoint
}

fn generated(mut endpoint: EndpointFact) -> EndpointFact {
    endpoint.generated_by = Some(APP_ENDPOINT_GENERATOR.to_owned());
    endpoint
}

fn reference(project: &str, name: &str, source: &str, endpoint: &str) -> ReferenceFact {
    ReferenceFact {
        project: project.to_owned(),
        name: name.to_owned(),
        source_project: source.to_owned(),
        endpoint: endpoint.to_owned(),
    }
}

fn slugs(found: &[&EndpointFact]) -> Vec<String> {
    found.iter().map(|endpoint| endpoint.slug.clone()).collect()
}

#[test]
fn the_apps_own_endpoint_serves_its_space_and_the_further_spaces_public_endpoints_by_name() {
    let app = app(&["alerts", "weather"]);
    let endpoints = vec![
        generated(endpoint("helsinki", "app-alerts", "s-own", "alerts")),
        endpoint("helsinki", "alerts-public", "s-alerts-public", "alerts"),
        // The further space is read through its public endpoints alone (AP-04, T-2933).
        public(endpoint("helsinki", "weather-z", "s-weather-z", "weather")),
        public(endpoint("helsinki", "weather-a", "s-weather-a", "weather")),
        endpoint(
            "helsinki",
            "weather-internal",
            "s-weather-internal",
            "weather",
        ),
        // Another project's space of the same name is not the App's.
        endpoint("espoo", "weather-b", "s-espoo", "weather"),
    ];
    let references = vec![
        reference("helsinki", "z-trams", "espoo", "trams"),
        reference("helsinki", "a-bikes", "espoo", "bikes"),
        // Another project's reference, a missing target and a target without a slug add nothing.
        reference("espoo", "a-other", "helsinki", "weather-a"),
        reference("helsinki", "m-gone", "espoo", "gone"),
        reference("helsinki", "n-unslugged", "espoo", "unslugged"),
    ];
    let endpoints = [
        endpoints,
        vec![
            endpoint("espoo", "trams", "s-trams", "trams"),
            endpoint("espoo", "bikes", "s-bikes", "bikes"),
            endpoint("espoo", "unslugged", "", "x"),
        ],
    ]
    .concat();
    let found = served_endpoints("helsinki", "alerts", &app.spec, &endpoints, &references);
    assert_eq!(
        slugs(&found),
        ["s-own", "s-weather-a", "s-weather-z", "s-bikes", "s-trams"]
    );
}

#[test]
fn an_endpoint_named_like_the_apps_own_but_not_generated_is_one_of_the_space() {
    let app = app(&["alerts"]);
    let endpoints = vec![
        endpoint("helsinki", "app-alerts", "s-hand", "alerts"),
        endpoint("helsinki", "alerts-public", "s-public", "alerts"),
    ];
    let found = served_endpoints("helsinki", "alerts", &app.spec, &endpoints, &[]);
    assert_eq!(slugs(&found), ["s-public", "s-hand"]);
}

#[test]
fn the_own_endpoint_of_another_space_does_not_stand_in_for_this_one() {
    let app = app(&["weather"]);
    let endpoints = vec![
        generated(endpoint("helsinki", "app-alerts", "s-own", "alerts")),
        endpoint("helsinki", "weather", "s-weather", "weather"),
    ];
    let found = served_endpoints("helsinki", "alerts", &app.spec, &endpoints, &[]);
    assert_eq!(slugs(&found), ["s-weather"]);
}

#[test]
fn each_slug_is_offered_once_and_no_more_than_five() {
    let app = app(&["alerts"]);
    let endpoints: Vec<EndpointFact> = (0..8)
        .map(|n| endpoint("helsinki", &format!("e{n}"), &format!("s{n}"), "alerts"))
        .collect();
    // The same Endpoint reached again through a reference is one audience, not two.
    let references = vec![reference("helsinki", "again", "helsinki", "e0")];
    let found = served_endpoints("helsinki", "alerts", &app.spec, &endpoints, &references);
    assert_eq!(found.len(), MAX_APP_ENDPOINTS);
    assert_eq!(slugs(&found), ["s0", "s1", "s2", "s3", "s4"]);
}

#[test]
fn an_app_whose_spaces_have_no_endpoint_reads_nothing() {
    let app = app(&["alerts"]);
    let endpoints = vec![endpoint("helsinki", "weather", "s-weather", "weather")];
    assert!(served_endpoints("helsinki", "alerts", &app.spec, &endpoints, &[]).is_empty());
    assert!(served_endpoints("helsinki", "alerts", &app.spec, &[], &[]).is_empty());
}

/// AP-04 (T-2933): the first need's space is the App's own; until its generated endpoint is
/// committed every endpoint of that space serves it. A further space offers only what already
/// answers anyone, so naming it in `dataNeeds` reaches no endpoint that is not public.
#[test]
fn a_further_space_without_a_public_endpoint_is_read_through_nothing() {
    let app = app(&["alerts", "weather"]);
    let endpoints = vec![
        endpoint("helsinki", "alerts-internal", "s-alerts", "alerts"),
        endpoint("helsinki", "weather-internal", "s-weather", "weather"),
    ];
    let found = served_endpoints("helsinki", "alerts", &app.spec, &endpoints, &[]);
    assert_eq!(slugs(&found), ["s-alerts"]);
}
