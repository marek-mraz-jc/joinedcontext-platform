//! Edge cases of `translators::ogc::bbox` (T-1952, EP-34, EP-26, MP-02).
//!
//! Contract, in one sentence: a `bbox` of four or six numbers becomes the three NGSI-LD parameters
//! of an `intersects` query over the horizontal box, and anything else is a named `400` — never a
//! filter that is silently dropped, because a filter that is ignored returns the rows the caller
//! asked not to see.
//!
//! The elevation of the six-value form is dropped on purpose: NGSI-LD's `geoQ` is two-dimensional,
//! so the box can only be wider than the caller asked for, and a wider box is filtered by the
//! client rather than hiding rows from it. What may never happen is the opposite — a `bbox` the
//! gateway accepts and turns into a query the broker ignores.

use context_gateway::translators::ogc::bbox;

fn parameters(raw: &str) -> Vec<(String, String)> {
    bbox(raw).unwrap_or_else(|error| panic!("{raw:?} is a bbox: {error}"))
}

fn coordinates(raw: &str) -> String {
    parameters(raw)
        .into_iter()
        .find(|(name, _)| name == "coordinates")
        .expect("a coordinates parameter")
        .1
}

/// EP-34: the box becomes a closed ring in CRS84 order, `intersects` so a feature that reaches
/// into the box counts, as OGC requires of `bbox`.
#[test]
fn four_numbers_become_an_intersects_polygon() {
    assert_eq!(
        parameters("19.1,48.7,19.2,48.8"),
        vec![
            ("georel".to_owned(), "intersects".to_owned()),
            ("geometry".to_owned(), "Polygon".to_owned()),
            (
                "coordinates".to_owned(),
                "[[[19.1,48.7],[19.2,48.7],[19.2,48.8],[19.1,48.8],[19.1,48.7]]]".to_owned()
            ),
        ]
    );
}

/// The six-value form carries a minimum and a maximum elevation between the horizontal corners;
/// the horizontal box is what survives.
#[test]
fn six_numbers_keep_the_horizontal_box_and_drop_the_elevation() {
    assert_eq!(
        coordinates("19.1,48.7,100.0,19.2,48.8,500.0"),
        coordinates("19.1,48.7,19.2,48.8"),
        "the same box, whether the caller gave an elevation or not"
    );
}

/// A count that is neither four nor six is a named refusal, not a guess.
#[test]
fn any_other_count_of_numbers_is_refused_by_name() {
    for raw in [
        "",
        "1",
        "1,2",
        "1,2,3",
        "1,2,3,4,5",
        "1,2,3,4,5,6,7",
        ",,,",
        "1,2,3,4,",
    ] {
        let error = bbox(raw).expect_err(&format!("{raw:?} is not a bbox"));
        assert_eq!(error.parameter, "bbox", "{raw:?}");
    }
}

/// A comma is the separator and nothing else: `19,1,48,7` is four whole numbers and an ordinary
/// box, not one decimal point misread. Written down because a client that formats numbers for a
/// Slovak locale sends exactly this, and it is accepted as the box it literally is.
#[test]
fn a_comma_is_a_separator_and_never_a_decimal_point() {
    assert_eq!(
        coordinates("19,1,48,7"),
        "[[[19.0,1.0],[48.0,1.0],[48.0,7.0],[19.0,7.0],[19.0,1.0]]]"
    );
}

/// A value that is not a number is refused, whatever it looks like.
#[test]
fn a_value_that_is_not_a_number_is_refused() {
    for raw in [
        "a,b,c,d",
        "19.1,48.7,19.2,east",
        "19.1,48.7,19.2,",
        "19.1;48.7;19.2;48.8",
        "19.1 48.7 19.2 48.8",
        "0x13,48.7,19.2,48.8",
        "19.1,48.7,19.2,48.8;drop",
    ] {
        let error = bbox(raw).expect_err(&format!("{raw:?} is not a bbox"));
        assert_eq!(error.parameter, "bbox", "{raw:?}");
        assert!(
            !error.detail.is_empty(),
            "a client has to be told what is wrong"
        );
    }
}

/// Space around a number is a client's formatting, not an error.
#[test]
fn whitespace_around_a_number_is_ignored() {
    assert_eq!(
        coordinates(" 19.1 , 48.7 , 19.2 , 48.8 "),
        coordinates("19.1,48.7,19.2,48.8")
    );
}

/// A corner the wrong way round is refused rather than silently swapped: a caller who wrote
/// `maxx,minx` asked for something, and a box the gateway invents is not it.
#[test]
fn a_lower_corner_greater_than_the_upper_one_is_refused() {
    for raw in [
        "19.2,48.7,19.1,48.8",
        "19.1,48.8,19.2,48.7",
        "19.2,48.8,19.1,48.7",
    ] {
        let error = bbox(raw).expect_err(&format!("{raw:?} is inverted"));
        assert_eq!(error.parameter, "bbox");
    }
}

/// A degenerate box — a line or a point — is a legitimate question and stays one.
#[test]
fn a_box_of_zero_width_or_height_is_allowed() {
    for raw in [
        "19.1,48.7,19.1,48.8",
        "19.1,48.7,19.2,48.7",
        "19.1,48.7,19.1,48.7",
    ] {
        assert_eq!(parameters(raw).len(), 3, "{raw}");
    }
}

/// Negative coordinates and the edges of the world are ordinary boxes.
#[test]
fn the_whole_world_and_the_southern_hemisphere_are_boxes() {
    assert_eq!(parameters("-180,-90,180,90").len(), 3);
    assert_eq!(parameters("-1.5,-2.5,-0.5,-1.5").len(), 3);
    assert_eq!(parameters("0,0,0,0").len(), 3);
}

/// A number that is not a place — `NaN`, an infinity — parses as an f64 and used to pass the
/// corner check, because every comparison with `NaN` is false; the ring then carried `null` where
/// a coordinate belongs, which is a `geoQ` no broker can honour (T-2339).
#[test]
fn a_bbox_of_nan_or_infinity_is_refused_by_name() {
    for raw in [
        "nan,nan,nan,nan",
        "NaN,48.7,19.2,48.8",
        "inf,48.7,19.2,48.8",
        "19.1,48.7,inf,inf",
        "-inf,-inf,inf,inf",
    ] {
        let error = bbox(raw).expect_err(&format!("{raw:?} is not a place"));
        assert_eq!(error.parameter, "bbox", "{raw:?}");
    }
}

/// A very long list is refused by its count before anything is parsed twice.
#[test]
fn a_long_list_of_numbers_is_refused_by_its_count() {
    let many = (0..1024)
        .map(|n| n.to_string())
        .collect::<Vec<_>>()
        .join(",");
    assert_eq!(bbox(&many).expect_err("not a bbox").parameter, "bbox");
}

/// The same input twice gives the same three parameters, and they are always in the order the
/// handler appends them to the upstream query.
#[test]
fn the_parameters_come_back_in_one_order() {
    let once = parameters("19.1,48.7,19.2,48.8");
    assert_eq!(once, parameters("19.1,48.7,19.2,48.8"));
    let names: Vec<&str> = once.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(names, vec!["georel", "geometry", "coordinates"]);
}
