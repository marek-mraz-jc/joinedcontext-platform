//! The `{space}` segment of an entity id is rendered, not written (PF-84, T-1446).

use jc_core::kinds::{urn_segment, ContextSpace};

fn space(yaml: &str) -> jc_core::error::Result<ContextSpace> {
    let parsed = ContextSpace::from_yaml(yaml).expect("the manifest parses");
    parsed.validate().map(|()| parsed)
}

fn manifest(namespace: &str, name: &str, pin: Option<&str>) -> String {
    let pin = pin
        .map(|pin| format!("  urnSegment: \"{pin}\"\n"))
        .unwrap_or_default();
    format!(
        "apiVersion: joinedcontext.com/v1alpha1\nkind: ContextSpace\nmetadata:\n  name: {name}\n  namespace: {namespace}\nspec:\n  isSandbox: false\n{pin}"
    )
}

#[test]
fn no_pin_renders_project_dash_name() {
    assert_eq!(urn_segment("helsinki", "bikes", None), "helsinki-bikes");
    assert!(space(&manifest("helsinki", "bikes", None)).is_ok());
}

#[test]
fn a_pin_wins() {
    assert_eq!(
        urn_segment("helsinki", "helsinki-hub", Some("helsinki-hub")),
        "helsinki-hub"
    );
    let parsed = space(&manifest("helsinki", "hub", Some("helsinki-hub"))).expect("valid");
    assert_eq!(parsed.spec.urn_segment.as_deref(), Some("helsinki-hub"));
}

#[test]
fn one_local_name_in_two_projects_renders_two_segments() {
    assert_ne!(
        urn_segment("helsinki", "air", None),
        urn_segment("espoo", "air", None)
    );
}

#[test]
fn a_pin_that_is_not_a_space_segment_is_refused() {
    for pin in ["Helsinki", "-hub", "hub:x", "a b", "", "ovzdušie"] {
        assert!(
            space(&manifest("helsinki", "hub", Some(pin))).is_err(),
            "{pin:?}"
        );
    }
}

#[test]
fn a_rendered_segment_longer_than_a_segment_may_be_is_refused() {
    let long = "a".repeat(40);
    assert!(space(&manifest(&long, &long, None)).is_err());
    assert!(space(&manifest(&long, &long, Some("short"))).is_ok());
}

#[test]
fn the_pin_round_trips_and_is_absent_when_not_set() {
    let parsed = space(&manifest("helsinki", "bikes", None)).expect("valid");
    let written = parsed.to_yaml().expect("serializes");
    assert!(!written.contains("urnSegment"), "{written}");
}
