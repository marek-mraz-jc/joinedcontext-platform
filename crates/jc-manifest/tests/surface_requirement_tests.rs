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
    repo_at(test_name, linkml, PUBLISHED, representations)
}

/// A published model commits its four generated artifacts (DM-02).
const PUBLISHED: &str = "published\n  artifacts:\n    jsonSchema: ./events.schema.json\n    \
                         context: ./events.context.jsonld\n    docs: ./events.md\n    \
                         example: ./events.example.jsonld";

fn repo_at(
    test_name: &str,
    linkml: Option<&str>,
    lifecycle: &str,
    representations: &str,
) -> PathBuf {
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
            &format!(
                "apiVersion: joinedcontext.com/v1alpha1\nkind: DataModel\nmetadata:\n  name: events\n  \
                 namespace: city\nspec:\n  contextSpaceRef: events\n  linkml: ./events.linkml.yaml\n  \
                 version: 1.0.0\n  lifecycle: {lifecycle}\n"
            ),
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
        ["Endpoint city/calendar, space events: `ical` maps the type Event, which the data model does not have (EP-125)"]
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
        ["Endpoint city/calendar, space events: `ical` maps DTSTART to `startDate`, which is not a required attribute of Event: mark it required or end a firstOf with a required one (EP-125)"]
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
    assert!(
        found[0].contains("maps the type Event, which the data model does not have"),
        "{}",
        found[0]
    );
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

/// T-4672: an Endpoint's `spec.display` names only types and slots of its projection (EP-135).
fn display_findings(dir: &Path) -> Vec<String> {
    validate::run(dir)
        .findings
        .into_iter()
        .filter(|f| f.message.contains("EP-135"))
        .map(|f| f.message)
        .collect()
}

#[test]
fn a_display_of_the_model_slots_is_accepted_and_an_unknown_name_refused() {
    let ok = "  display:\n    Event:\n      fields: [startDate, name]\n      summary: [name]\n";
    let dir = repo_with("display-ok", Some(EVENT_MODEL), ok);
    assert_eq!(display_findings(&dir), Vec::<String>::new());
    let _ = std::fs::remove_dir_all(&dir);

    let bad = "  display:\n    Event:\n      fields: [startDate, venue]\n    Meeting:\n      fields: [name]\n";
    let dir = repo_with("display-bad", Some(EVENT_MODEL), bad);
    assert_eq!(
        display_findings(&dir),
        [
            "Endpoint city/calendar: spec.display: venue is no slot of Event in the Endpoint's projection (EP-135)",
            "Endpoint city/calendar: spec.display: Meeting is no type of its projection (EP-135)",
        ]
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_display_is_checked_against_the_projection_not_the_whole_model() {
    let display = "  projectionRef:\n    kind: ModelProjection\n    name: public\n  display:\n    Event:\n      fields: [name, endDate]\n";
    let dir = repo_with("display-projection", Some(EVENT_MODEL), display);
    write(
        &dir,
        "projects/city/spaces/events/projections/public.yaml",
        "apiVersion: joinedcontext.com/v1alpha1\nkind: ModelProjection\nmetadata:\n  name: public\n  \
         namespace: city\nspec:\n  contextSpaceRef: events\n  dataModelRef:\n    name: events\n    \
         version: \"1\"\n  classes:\n    - name: Event\n      slots: [name, startDate]\n",
    );
    let found = display_findings(&dir);
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(
        found[0].contains("endDate is no slot of Event"),
        "{}",
        found[0]
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The mapping API/02 §8b prints, run through `jcctl validate` against the shared fixture model
/// (T-4900): accepted as printed, and refused with one finding per gap once broken.
const DOCS_MAPPING: &str = "  enabledRepresentations: [ngsi-ld, ical]
  representations:
    ical:
      type: Event
      preset: sdm-event
      mapping:
        LOCATION: { join: [address.streetAddress, address.addressLocality], separator: \", \" }
        SUMMARY: { attribute: name, language: sk, truncate: 200 }
        DTEND: { attribute: endDate, date: date, endInclusive: true }
";

#[test]
fn the_documented_mapping_validates_and_each_gap_is_one_finding() {
    let model = include_str!("../../../testdata/ical-mapping/model.linkml.yaml");
    let dir = repo_with("docs-mapping", Some(model), DOCS_MAPPING);
    assert_eq!(surface_findings(&dir), Vec::<String>::new());
    let _ = std::fs::remove_dir_all(&dir);

    let broken = DOCS_MAPPING
        .replace("preset: sdm-event", "preset: schema-org")
        .replace(
            "attribute: name, language",
            "attribute: description, language",
        )
        .replace("attribute: endDate,", "attribute: url,");
    let dir = repo_with("docs-mapping-broken", Some(model), &broken);
    assert_eq!(
        surface_findings(&dir),
        [
            "Endpoint city/calendar, space events: `ical` maps DTEND to `url`, range uri; DTEND takes date or datetime (EP-125)",
            "Endpoint city/calendar, space events: `ical` maps SUMMARY to `description`, which is not a required attribute of Event: mark it required or end a firstOf with a required one (EP-125)",
        ]
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// T-4886: an air-quality model, each pollutant a float with its UCUM unit (as the dev seed).
const AIR_MODEL: &str = r#"id: https://example.org/models/air
name: air
slots:
  name: {}
  dateObserved:
    range: datetime
  pm10:
    range: float
    unit:
      ucum_code: ug/m3
classes:
  AirQualityObserved:
    slots: [name, dateObserved, pm10]
"#;

const STA: &str = "  enabledRepresentations: [ngsi-ld, sta]\n";

#[test]
fn sta_on_a_model_with_a_measured_attribute_is_accepted() {
    let dir = repo_with("sta-air", Some(AIR_MODEL), STA);
    assert_eq!(surface_findings(&dir), Vec::<String>::new());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn sta_on_a_model_without_a_measurement_is_refused() {
    // Article: texts and dates; a number without a unit is no Datastream either (STA §9.2.5).
    let article = "classes:\n  Article:\n    attributes:\n      headline: {}\n      \
                   datePublished: { range: datetime }\n      wordCount: { range: integer }\n";
    let refusal = "Endpoint city/calendar, space events: `sta` needs a type with a numeric \
                   attribute that declares its unit (EP-125)";
    let dir = repo_with("sta-article", Some(article), STA);
    assert_eq!(surface_findings(&dir), [refusal]);
    let _ = std::fs::remove_dir_all(&dir);
    let dir = repo_with("sta-no-model", None, STA);
    assert_eq!(surface_findings(&dir), [refusal]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_draft_model_backs_no_domain_standard() {
    for (name, lifecycle) in [("draft", "draft"), ("retired", "retired")] {
        let dir = repo_at(name, Some(EVENT_MODEL), lifecycle, ICAL);
        assert_eq!(
            surface_findings(&dir),
            [format!(
                "Endpoint city/calendar: `ical` needs a published data model in space events; its \
                 model is {lifecycle} (EP-125, DM-26)"
            )]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[test]
fn a_published_model_wins_over_a_draft_of_the_next_version() {
    let dir = repo_with("published-and-draft", Some(EVENT_MODEL), ICAL);
    write(
        &dir,
        "projects/city/spaces/events/datamodels/events-v2.yaml",
        "apiVersion: joinedcontext.com/v1alpha1\nkind: DataModel\nmetadata:\n  name: events-v2\n  \
         namespace: city\nspec:\n  contextSpaceRef: events\n  linkml: ./events.linkml.yaml\n  \
         version: 2.0.0\n  lifecycle: draft\n",
    );
    assert_eq!(surface_findings(&dir), Vec::<String>::new());
    let _ = std::fs::remove_dir_all(&dir);
}
