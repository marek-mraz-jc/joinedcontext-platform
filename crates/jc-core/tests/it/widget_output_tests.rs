//! API/11 EP-163 (T-5239): the catalogue widget's map defaults an Endpoint lists in
//! `spec.representations.widget` are checked when it is saved, each refusal naming the output,
//! the class and the member; the gateway hands the first output's defaults per class to the widget.
//! EP-164 (T-5197): a source's `table` defaults, checked the same way.

use std::path::PathBuf;

use jc_core::kinds::{map_defaults, EndpointSpec};

fn model() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/ical-mapping/model.linkml.yaml");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn endpoint(widget: &str) -> Result<EndpointSpec, String> {
    let yaml = format!(
        "contextSpaceRef: events\nslug: abcdefghijklmnopqrstuvwxyz234567\naudience: public\n\
         representations:\n  widget:\n{widget}"
    );
    let spec: EndpointSpec = serde_norway::from_str(&yaml).map_err(|e| e.to_string())?;
    spec.validate().map_err(|e| e.to_string())?;
    Ok(spec)
}

fn model_gaps(widget: &str) -> Vec<String> {
    endpoint(widget)
        .expect("the shape is valid")
        .representation_gaps(Some(&model()))
        .expect("the model parses")
}

const VALID: &str = r#"    - name: mapa
      title: { sk: Mapa podujatí }
      sources:
        - type: Event
          mapping: { geometry: location, colour: category, popup: [name, startDate, address.addressLocality] }
          cluster: false
"#;

#[test]
fn a_class_with_geometry_colour_popup_and_clustering_passes_and_reaches_the_widget() {
    let spec = endpoint(VALID).expect("valid");
    assert_eq!(
        spec.representation_gaps(Some(&model())).unwrap(),
        Vec::<String>::new()
    );
    let defaults = map_defaults(spec.widget_outputs());
    let event = &defaults["Event"];
    assert_eq!(event.geometry.as_deref(), Some("location"));
    assert_eq!(event.colour.as_deref(), Some("category"));
    assert_eq!(
        event.popup,
        ["name", "startDate", "address.addressLocality"]
    );
    assert_eq!(event.cluster, Some(false));
    assert_eq!(
        serde_json::to_value(event).unwrap(),
        serde_json::json!({"geometry": "location", "colour": "category",
            "popup": ["name", "startDate", "address.addressLocality"], "cluster": false})
    );
}

#[test]
fn the_first_output_listing_a_class_sets_its_defaults() {
    let spec = endpoint(
        "    - name: prva\n      sources: [{ type: Event, mapping: { colour: eventStatus } }]\n\
         \x20   - name: druha\n      sources: [{ type: Event, mapping: { colour: category } }]\n",
    )
    .expect("valid");
    assert_eq!(
        map_defaults(spec.widget_outputs())["Event"]
            .colour
            .as_deref(),
        Some("eventStatus")
    );
}

/// One refusal per role and range, each naming the output, the class and the member.
#[test]
fn a_role_the_model_cannot_fill_is_refused_and_named() {
    for (mapping, says) in [
        ("{ geometry: startDate }", "widget output `mapa`, Event: `mapping.geometry` reads `startDate`, range datetime; geometry takes a GeoProperty"),
        ("{ geometry: nowhere }", "widget output `mapa`, Event: `mapping.geometry` names `nowhere`, which Event does not have"),
        ("{ colour: name }", "widget output `mapa`, Event: `mapping.colour` reads `name`, a LanguageProperty; colour takes an enum or a boolean"),
        ("{ colour: startDate }", "widget output `mapa`, Event: `mapping.colour` reads `startDate`, range datetime; colour takes an enum or a boolean"),
        ("{ colour: organizer }", "widget output `mapa`, Event: `mapping.colour` reads `organizer`, a Relationship; colour takes an enum or a boolean"),
        ("{ colour: nowhere }", "widget output `mapa`, Event: `mapping.colour` names `nowhere`, which Event does not have"),
        ("{ popup: [name, nowhere] }", "widget output `mapa`, Event: `mapping.popup` names `nowhere`, which Event does not have"),
        ("{ popup: [location] }", "widget output `mapa`, Event: `mapping.popup` names `location`, a GeoProperty; the map draws it, the popup shows values"),
    ] {
        let gaps = model_gaps(&format!("    - name: mapa\n      sources: [{{ type: Event, mapping: {mapping} }}]\n"));
        assert_eq!(gaps, [says], "{mapping}");
    }
    let gaps = model_gaps("    - name: mapa\n      sources: [{ type: Place }]\n");
    assert_eq!(
        gaps,
        ["widget output `mapa` lists the type Place, which the data model does not have"]
    );
}

/// The shape is refused before any model is read: at save, by the kind's own parse.
#[test]
fn a_malformed_output_is_refused_at_save_and_named() {
    for (widget, says) in [
        ("    - name: Mapa\n      sources: [{ type: Event }]\n", "`name` takes lowercase letters"),
        ("    - name: mapa\n      sources: []\n", "`sources` lists 1 to 10 classes"),
        ("    - name: mapa\n      sources: [{ type: Event }, { type: Event }]\n", "lists Event twice"),
        ("    - name: mapa\n      sources: [{ type: Event, mapping: { colour: 'a b' } }]\n", "`mapping.colour: a b` is not an attribute name"),
        ("    - name: mapa\n      sources: [{ type: Event, mapping: { popup: [name, name] } }]\n", "`mapping.popup` names `name` twice"),
        ("    - name: mapa\n      sources: [{ type: Event }]\n    - name: mapa\n      sources: [{ type: Event }]\n", "widget output `mapa` is listed twice"),
    ] {
        let err = endpoint(widget).expect_err(widget);
        assert!(err.contains("spec.representations.widget") && err.contains(says), "{err}");
    }
    let eleven: Vec<String> = (0..11).map(|i| format!("a{i}")).collect();
    let err = endpoint(&format!(
        "    - name: mapa\n      sources: [{{ type: Event, mapping: {{ popup: [{}] }} }}]\n",
        eleven.join(", ")
    ))
    .expect_err("eleven popup fields");
    assert!(
        err.contains("`mapping.popup` lists 11 attributes, at most 10"),
        "{err}"
    );
}

#[test]
fn an_unknown_member_or_a_cluster_that_is_no_boolean_does_not_parse() {
    for widget in [
        "    - name: mapa\n      sources: [{ type: Event, mapping: { size: name } }]\n",
        "    - name: mapa\n      sources: [{ type: Event, cluster: often }]\n",
        "    - name: mapa\n      zoom: 3\n      sources: [{ type: Event }]\n",
    ] {
        assert!(endpoint(widget).is_err(), "{widget}");
    }
}

const TABLE: &str = r#"    - name: tabulka
      sources:
        - type: Event
          table: { filter: 'startDate>=today;startDate<today+30d', orderBy: startDate;desc, pageLength: 50, columns: [name, startDate, address.addressLocality] }
"#;

#[test]
fn a_table_start_passes_and_reaches_the_widget_beside_the_map() {
    let spec = endpoint(TABLE).expect("valid");
    assert_eq!(
        spec.representation_gaps(Some(&model())).unwrap(),
        Vec::<String>::new()
    );
    let defaults = map_defaults(spec.widget_outputs());
    let table = defaults["Event"].table.as_ref().expect("a table start");
    assert_eq!(table.order_attribute(), Some("startDate"));
    assert_eq!(
        serde_json::to_value(table).unwrap(),
        serde_json::json!({"filter": "startDate>=today;startDate<today+30d", "orderBy": "startDate;desc",
            "pageLength": 50, "columns": ["name", "startDate", "address.addressLocality"]})
    );
    // The map's own answer never carries it: widget.json lists it under `table`.
    assert_eq!(
        serde_json::to_value(&defaults["Event"]).unwrap(),
        serde_json::json!({})
    );
}

#[test]
fn a_table_start_the_widget_cannot_use_is_refused_at_save_and_named() {
    for (table, says) in [
        (
            "{ filter: 'startDate>=>today' }",
            "`table.filter` is not an NGSI-LD q",
        ),
        (
            "{ filter: 'todays==1;(' }",
            "`table.filter` is not an NGSI-LD q",
        ),
        (
            "{ orderBy: 'start date' }",
            "`table.orderBy: start date` is not an attribute name",
        ),
        (
            "{ pageLength: 25 }",
            "`table.pageLength: 25` is not one of 10, 20, 50 or 100",
        ),
        (
            "{ columns: [name, name] }",
            "`table.columns` names `name` twice",
        ),
        (
            "{ columns: ['a b'] }",
            "`table.columns` names `a b`, which is not an attribute name",
        ),
    ] {
        let err = endpoint(&format!(
            "    - name: tabulka\n      sources: [{{ type: Event, table: {table} }}]\n"
        ))
        .expect_err(table);
        assert!(
            err.contains("widget output `tabulka`, Event") && err.contains(says),
            "{err}"
        );
    }
    let long = format!("name==\"{}\"", "a".repeat(1000));
    let err = endpoint(&format!(
        "    - name: tabulka\n      sources: [{{ type: Event, table: {{ filter: '{long}' }} }}]\n"
    ))
    .expect_err("a long filter");
    assert!(err.contains("at most 1000 characters"), "{err}");
    assert!(endpoint(
        "    - name: tabulka\n      sources: [{ type: Event, table: { sort: name } }]\n"
    )
    .is_err());
}

#[test]
fn a_table_column_or_order_the_class_lacks_or_draws_on_the_map_is_refused() {
    for (table, says) in [
        ("{ orderBy: nowhere }", "widget output `tabulka`, Event: `table.orderBy` names `nowhere`, which Event does not have"),
        ("{ columns: [name, nowhere] }", "widget output `tabulka`, Event: `table.columns` names `nowhere`, which Event does not have"),
        ("{ columns: [location] }", "widget output `tabulka`, Event: `table.columns` names `location`, a GeoProperty; the map draws it, the table shows values"),
    ] {
        let gaps = model_gaps(&format!("    - name: tabulka\n      sources: [{{ type: Event, table: {table} }}]\n"));
        assert_eq!(gaps, [says], "{table}");
    }
}
