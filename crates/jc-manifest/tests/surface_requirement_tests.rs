//! T-4573: `jcctl validate` refuses an Endpoint that lists a domain-standard representation
//! (`ical`) when its space's data model lacks the type that standard maps, or has it without
//! each mandatory source attribute marked required, and names what is missing (EP-125).

use jc_manifest::validate;
use std::path::{Path, PathBuf};

fn temp_repo(test_name: &str) -> PathBuf {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after the epoch")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "jcctl-surface-{test_name}-{}-{now}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp repo");
    dir
}

fn write(dir: &Path, rel: &str, body: &str) {
    let path = dir.join(rel);
    std::fs::create_dir_all(path.parent().expect("relative path has a parent"))
        .expect("create parent");
    std::fs::write(path, body).expect("write manifest");
}

const ENDPOINT_PATH: &str = "projects/city/spaces/events/endpoints/calendar.yaml";

/// The SDM Event as a city models it: `startDate` required on the slot, `name` in slot_usage.
const EVENT_MODEL: &str = r#"id: https://example.org/models/events
name: events
slots:
  name: {}
  startDate:
    range: datetime
    required: true
  endDate:
    range: datetime
classes:
  Event:
    class_uri: https://smartdatamodels.org/dataModel.Event/Event
    slots: [name, startDate, endDate]
    slot_usage:
      name:
        required: true
"#;

fn repo_with(test_name: &str, linkml: Option<&str>, representations: &str) -> PathBuf {
    let dir = temp_repo(test_name);
    write(
        &dir,
        "org.yaml",
        r#"apiVersion: joinedcontext.com/v1alpha1
kind: Organization
metadata:
  name: city
  namespace: org
spec:
  domain: city.example
  locales: ["en"]
  defaultLocale: en
"#,
    );
    write(
        &dir,
        "projects/city/project.yaml",
        r#"apiVersion: joinedcontext.com/v1alpha1
kind: Project
metadata:
  name: city
  namespace: org
spec:
  organizationRef: city
"#,
    );
    write(
        &dir,
        "projects/city/spaces/events/space.yaml",
        r#"apiVersion: joinedcontext.com/v1alpha1
kind: ContextSpace
metadata:
  name: events
  namespace: city
spec:
  isSandbox: false
"#,
    );
    if let Some(linkml) = linkml {
        write(
            &dir,
            "projects/city/spaces/events/datamodels/events.yaml",
            r#"apiVersion: joinedcontext.com/v1alpha1
kind: DataModel
metadata:
  name: events
  namespace: city
spec:
  contextSpaceRef: events
  linkml: ./events.linkml.yaml
  version: 1.0.0
  lifecycle: draft
"#,
        );
        write(
            &dir,
            "projects/city/spaces/events/datamodels/events.linkml.yaml",
            linkml,
        );
    }
    write(
        &dir,
        ENDPOINT_PATH,
        &format!(
            "apiVersion: joinedcontext.com/v1alpha1\nkind: Endpoint\nmetadata:\n  name: calendar\n  \
             namespace: city\nspec:\n  contextSpaceRef: events\n  slug: k7m2qz4tv6xh3n5jb2ryd3wcfa\n  \
             audience: public\n{representations}"
        ),
    );
    dir
}

fn surface_findings(dir: &Path) -> Vec<String> {
    validate::run(dir)
        .findings
        .into_iter()
        .filter(|f| f.message.contains("EP-125"))
        .map(|f| {
            assert_eq!(f.path, Path::new(ENDPOINT_PATH));
            f.message
        })
        .collect()
}

const ICAL: &str = "  enabledRepresentations: [ngsi-ld, ical]\n";

#[test]
fn the_sdm_event_with_its_mandatory_attributes_required_is_accepted() {
    let dir = repo_with("accepted", Some(EVENT_MODEL), ICAL);
    assert_eq!(surface_findings(&dir), Vec::<String>::new());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_model_without_event_is_refused_and_the_type_named() {
    let model = EVENT_MODEL.replace("  Event:", "  Meeting:");
    let dir = repo_with("no-event", Some(&model), ICAL);
    assert_eq!(
        surface_findings(&dir),
        ["Endpoint city/calendar: `ical` needs the type Event in the data model of space events (EP-125)"]
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_event_whose_start_date_is_not_required_is_refused_and_only_that_attribute_named() {
    let model = EVENT_MODEL.replace(
        "    range: datetime\n    required: true\n",
        "    range: datetime\n",
    );
    let dir = repo_with("optional-start", Some(&model), ICAL);
    assert_eq!(
        surface_findings(&dir),
        ["Endpoint city/calendar: `ical` needs the type Event with the required attribute(s) startDate in the data model of space events (EP-125)"]
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn slot_usage_can_take_the_requirement_back() {
    let model = EVENT_MODEL.replace(
        "    slot_usage:\n      name:\n        required: true\n",
        "    slot_usage:\n      name:\n        required: true\n      startDate:\n        required: false\n",
    );
    let dir = repo_with("usage-false", Some(&model), ICAL);
    let found = surface_findings(&dir);
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(
        found[0].contains("startDate") && !found[0].contains("name"),
        "{}",
        found[0]
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_space_without_a_model_has_no_event() {
    let dir = repo_with("no-model", None, ICAL);
    let found = surface_findings(&dir);
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].contains("needs the type Event"), "{}", found[0]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_endpoint_that_does_not_list_ical_is_not_held_to_it() {
    // The default serves every generic representation and never `ical` (EP-104, EP-125).
    let dir = repo_with(
        "default",
        Some(&EVENT_MODEL.replace("  Event:", "  Meeting:")),
        "",
    );
    assert_eq!(surface_findings(&dir), Vec::<String>::new());
    let _ = std::fs::remove_dir_all(&dir);
}
