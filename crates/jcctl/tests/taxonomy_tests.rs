//! T-0873: the seeded role taxonomy (PF-56, PF-71) — what each role grants, and that a seed
//! never overwrites a role a person has edited.

use jc_core::kinds::{RoleSpec, Verb};
use jcctl::taxonomy;

fn role(name: &str) -> RoleSpec {
    let (_, content) = taxonomy::files()
        .into_iter()
        .find(|(path, _)| path == &format!("users/roles/{name}.yaml"))
        .unwrap_or_else(|| panic!("the taxonomy seeds {name}"));
    let manifest: serde_json::Value = serde_norway::from_str(&content).expect("parses as yaml");
    serde_json::from_value(manifest["spec"].clone()).expect("a role spec")
}

#[test]
fn every_seeded_role_is_a_valid_role_manifest() {
    for (path, content) in taxonomy::files() {
        let result = jc_core::registry::validate_yaml("Role", &content)
            .unwrap_or_else(|| panic!("{path} is a Role"));
        result.unwrap_or_else(|err| panic!("{path} does not validate: {err}"));
        assert!(
            content.starts_with("# "),
            "{path} says what the role is for"
        );
        assert!(content.contains("namespace: org"), "{path}");
    }
    let names: Vec<&str> = taxonomy::taxonomy()
        .iter()
        .map(|seeded| seeded.name)
        .collect();
    assert_eq!(
        names,
        vec![
            "viewer",
            "model-editor",
            "pipeline-editor",
            "endpoint-editor",
            "app-editor",
            "steward",
            "publisher",
            "people-admin",
            "org-admin"
        ]
    );
}

#[test]
fn publisher_approves_public_endpoints_and_reads_everything_else() {
    let publisher = role("publisher");
    let reads = publisher
        .rules
        .iter()
        .find(|rule| rule.verbs == vec![Verb::Read])
        .expect("publisher reads the project");
    assert!(reads.kinds.contains(&"Endpoint".to_owned()));
    assert!(reads.kinds.contains(&"Pipeline".to_owned()));
    assert!(reads.constraints.is_empty());

    let approves = publisher
        .rules
        .iter()
        .find(|rule| rule.verbs.contains(&Verb::Approve))
        .expect("publisher approves something");
    assert_eq!(approves.kinds, vec!["Endpoint".to_owned()]);
    assert_eq!(approves.verbs, vec![Verb::Approve]);
    assert_eq!(approves.constraints.len(), 1);
    assert_eq!(approves.constraints[0].field, "spec.audience");
    assert_eq!(approves.constraints[0].one_of, vec!["public".to_owned()]);
    // Publishing is a right of its own: it carries no propose and no delete.
    assert!(publisher
        .rules
        .iter()
        .all(|rule| !rule.verbs.contains(&Verb::Delete) && !rule.verbs.contains(&Verb::Propose)));
}

#[test]
fn the_steward_approves_everything_but_letting_data_out_to_the_public() {
    let steward = role("steward");
    let endpoints = steward
        .rules
        .iter()
        .find(|rule| rule.kinds == vec!["Endpoint".to_owned()])
        .expect("the endpoint rule stands alone");
    assert_eq!(endpoints.constraints.len(), 1);
    assert_eq!(endpoints.constraints[0].not_in, vec!["public".to_owned()]);
    assert!(endpoints.verbs.contains(&Verb::Approve));

    // Every other project kind is the steward's, unconstrained, and Endpoint is not among them.
    let rest = steward
        .rules
        .iter()
        .find(|rule| rule.kinds.len() > 1)
        .expect("the rest of the project");
    assert!(!rest.kinds.contains(&"Endpoint".to_owned()));
    assert!(rest.constraints.is_empty());
    assert!(rest.kinds.contains(&"Pipeline".to_owned()));
    // A steward writes no roles and no bindings: those are the organization's (PF-56).
    assert!(steward
        .rules
        .iter()
        .all(|rule| !rule.kinds.contains(&"RoleBinding".to_owned())));
}

#[test]
fn only_org_admin_approves_a_public_endpoint_unconstrained() {
    for seeded in taxonomy::taxonomy() {
        if seeded.name == "org-admin" {
            let rule = &seeded.rules[0];
            assert!(rule.verbs.contains(&Verb::Approve) && rule.verbs.contains(&Verb::Delete));
            assert!(rule.constraints.is_empty());
            assert!(rule.kinds.contains(&"Role".to_owned()));
            assert!(rule.kinds.contains(&"RoleBinding".to_owned()));
            continue;
        }
        for rule in &seeded.rules {
            let approves_endpoints =
                rule.verbs.contains(&Verb::Approve) && rule.kinds.contains(&"Endpoint".to_owned());
            assert!(
                !approves_endpoints || !rule.constraints.is_empty(),
                "{} approves an Endpoint with no constraint on its audience (PF-71)",
                seeded.name
            );
        }
    }
}

/// PF-91: `people-admin` holds every verb on Person and nothing else; `org-admin` holds it too.
#[test]
fn people_admin_manages_people_and_org_admin_holds_it() {
    let people = vec![
        Verb::Read,
        Verb::Create,
        Verb::Update,
        Verb::Disable,
        Verb::Delete,
    ];
    let admin = role("people-admin");
    assert_eq!(admin.rules.len(), 1);
    assert_eq!(admin.rules[0].kinds, vec!["Person".to_owned()]);
    assert_eq!(admin.rules[0].verbs, people);

    let org = role("org-admin");
    assert!(
        org.rules
            .iter()
            .any(|rule| rule.kinds == vec!["Person".to_owned()] && rule.verbs == people),
        "org-admin holds people-admin's rule"
    );
    for name in ["viewer", "steward", "publisher", "app-editor"] {
        assert!(
            role(name)
                .rules
                .iter()
                .all(|rule| !rule.kinds.contains(&"Person".to_owned())),
            "{name} reaches no person"
        );
    }
}

#[test]
fn an_editor_never_approves_and_never_deletes() {
    for name in [
        "viewer",
        "model-editor",
        "pipeline-editor",
        "endpoint-editor",
        "app-editor",
    ] {
        let spec = role(name);
        for rule in &spec.rules {
            assert!(
                !rule.verbs.contains(&Verb::Approve) && !rule.verbs.contains(&Verb::Delete),
                "{name} carries a verb an editor never holds (PF-56)"
            );
        }
    }
}

#[test]
fn seeding_twice_writes_nothing_the_second_time_and_leaves_an_edited_role_alone() {
    let dir = std::env::temp_dir().join(format!("jcctl-seed-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a temporary repository");

    let written = taxonomy::seed(&dir).expect("seeds");
    assert_eq!(written.len(), 9);
    let steward = dir.join("users/roles/steward.yaml");
    std::fs::write(&steward, "# edited by a person\n").expect("edit the steward");

    let again = taxonomy::seed(&dir).expect("seeds again");
    assert!(again.is_empty(), "{again:?}");
    assert_eq!(
        std::fs::read_to_string(&steward).expect("read"),
        "# edited by a person\n"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// AP-120, PF-71 (T-2690): a public App is the publisher's to approve, as a public Endpoint is. The
/// steward approves every other App, one without `spec.visibility` included (it is `project`), and
/// still proposes a public one: writing it is the author's, letting it out is the publisher's.
#[test]
fn a_public_app_is_the_publishers_to_approve_and_every_other_app_the_stewards() {
    let approves = |name: &str, visibility: Option<&str>| {
        role(name).rules.iter().any(|rule| {
            rule.kinds.contains(&"App".to_owned())
                && rule.verbs.contains(&Verb::Approve)
                && rule.constraints.iter().all(|c| c.holds(visibility))
        })
    };
    assert!(approves("publisher", Some("public")));
    assert!(!approves("publisher", Some("project")));
    assert!(!approves("publisher", None));
    assert!(!approves("steward", Some("public")));
    assert!(approves("steward", Some("project")));
    assert!(approves("steward", Some("roles")));
    assert!(
        approves("steward", None),
        "an App without visibility is project (AP-120)"
    );

    let steward_proposes_apps = role("steward").rules.iter().any(|rule| {
        rule.kinds == vec!["App".to_owned()]
            && rule.verbs == vec![Verb::Propose]
            && rule.constraints.is_empty()
    });
    assert!(
        steward_proposes_apps,
        "a steward proposes any App, public ones too"
    );
    // The steward's unconstrained rule leaves App out, or it would approve a public one.
    assert!(role("steward")
        .rules
        .iter()
        .filter(|rule| rule.constraints.is_empty() && rule.verbs.contains(&Verb::Approve))
        .all(|rule| !rule.kinds.contains(&"App".to_owned())));
}
