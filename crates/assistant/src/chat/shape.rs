//! The area of a polygon answer (T-4697): a parcel's, a municipality's or a district's area and
//! perimeter, measured geodesically on the WGS84 ellipsoid (Karney) from the geometry the Endpoint
//! answered, and its outline simplified for the map alone. The model reads the area as a column;
//! it never reads a coordinate.

use geo::orient::Direction;
use geo::{
    BoundingRect, Coord, GeodesicArea, LineString, MultiPolygon, Orient, Polygon, Simplify,
    Validation,
};
use serde_json::{json, Value};
use std::collections::BTreeMap;

use super::agent::Table;

/// The finest a displayed outline is simplified to: 1e-5° is about a metre.
const FINEST: f64 = 1e-5;
/// A displayed outline keeps detail of about this share of its extent.
const DETAIL: f64 = 1e-3;

/// A GeoJSON Polygon or MultiPolygon as rings of positions; `None` for any other geometry.
fn polygons(geometry: &Value) -> Option<MultiPolygon> {
    let polygon = |rings: &Value| -> Option<Polygon> {
        let mut rings = rings.as_array()?.iter().map(|ring| {
            ring.as_array()?
                .iter()
                .map(|p| {
                    Some(Coord {
                        x: p.get(0)?.as_f64()?,
                        y: p.get(1)?.as_f64()?,
                    })
                })
                .collect::<Option<Vec<_>>>()
                .map(LineString::from)
        });
        let exterior = rings.next()??;
        Some(Polygon::new(exterior, rings.collect::<Option<_>>()?))
    };
    let coordinates = geometry.get("coordinates")?;
    match geometry.get("type")?.as_str()? {
        "Polygon" => Some(MultiPolygon::new(vec![polygon(coordinates)?])),
        "MultiPolygon" => coordinates
            .as_array()?
            .iter()
            .map(polygon)
            .collect::<Option<Vec<_>>>()
            .map(MultiPolygon::new),
        _ => None,
    }
}

/// The area in m² and the perimeter in m of a Polygon or MultiPolygon, its holes subtracted from
/// the area and their rings counted in the perimeter, whichever way its rings wind; `None` for
/// any other geometry.
pub fn measure(geometry: &Value) -> Option<(f64, f64)> {
    let shape = polygons(geometry)?.orient(Direction::Default);
    let (perimeter, area) = shape.geodesic_perimeter_area_signed();
    Some((area.abs(), perimeter))
}

fn rings(polygon: &Polygon) -> Value {
    std::iter::once(polygon.exterior())
        .chain(polygon.interiors())
        .map(|ring| ring.coords().map(|c| json!([c.x, c.y])).collect::<Value>())
        .collect()
}

/// `geometry` as the map draws it: a Polygon or MultiPolygon simplified (Douglas-Peucker) to a
/// thousandth of its extent and never finer than a metre, so a district loses its metres and a
/// parcel keeps them; kept as it is when the simpler outline would not be a valid polygon. Any
/// other geometry is returned as it is.
pub fn for_display(geometry: &Value) -> Value {
    let Some(shape) = polygons(geometry) else {
        return geometry.clone();
    };
    let Some(extent) = shape.bounding_rect().map(|r| r.width().max(r.height())) else {
        return geometry.clone();
    };
    let simple = shape.simplify((extent * DETAIL).max(FINEST));
    if simple.0.len() != shape.0.len() || !simple.is_valid() {
        return geometry.clone();
    }
    match (
        geometry.get("type").and_then(Value::as_str),
        simple.0.first(),
    ) {
        (Some("Polygon"), Some(polygon)) => {
            json!({"type": "Polygon", "coordinates": rings(polygon)})
        }
        _ => {
            json!({"type": "MultiPolygon", "coordinates": simple.0.iter().map(rings).collect::<Vec<_>>()})
        }
    }
}

/// The unit a column of areas reads in, by its largest: m² under a hectare, ha under a km², else
/// km²; with the divisor from m².
pub fn area_unit(largest: f64) -> (&'static str, f64) {
    if largest < 1e4 {
        ("m²", 1.0)
    } else if largest < 1e6 {
        ("ha", 1e4)
    } else {
        ("km²", 1e6)
    }
}

/// The unit a column of perimeters reads in, by its largest: m under a kilometre, else km.
pub fn length_unit(largest: f64) -> (&'static str, f64) {
    if largest < 1e3 {
        ("m", 1.0)
    } else {
        ("km", 1e3)
    }
}

/// `x` to three significant figures at most, never fewer than its whole digits.
pub fn rounded(x: f64) -> f64 {
    if x == 0.0 || !x.is_finite() {
        return 0.0;
    }
    let digits = (2 - x.abs().log10().floor() as i32).max(0);
    let scale = 10f64.powi(digits);
    (x * scale).round() / scale
}

/// The words of the two columns [`measured`] adds, per language.
fn words(pairs: &[(&str, &str)]) -> Option<BTreeMap<String, String>> {
    Some(
        pairs
            .iter()
            .map(|(l, w)| ((*l).to_owned(), (*w).to_owned()))
            .collect(),
    )
}

/// Adds `area` and `perimeter` columns to a table whose rows have polygons, each in the unit of
/// its largest value and to three significant figures, and replaces those rows' outlines with
/// [`for_display`]'s. Rows without a polygon read empty cells. Returns whether any row had one; a
/// table without a polygon is left as it is.
pub fn measured(table: &mut Table) -> bool {
    let measures: Vec<Option<(f64, f64)>> = table
        .geometries
        .iter()
        .map(|g| g.as_ref().and_then(measure))
        .collect();
    let largest =
        |pick: fn(&(f64, f64)) -> f64| measures.iter().flatten().map(pick).reduce(f64::max);
    let (Some(area), Some(length)) = (largest(|m| m.0), largest(|m| m.1)) else {
        return false;
    };
    let ((area_unit, per_area), (length_unit, per_length)) = (area_unit(area), length_unit(length));
    for (i, row) in table.rows.iter_mut().enumerate() {
        let m = measures.get(i).copied().flatten();
        row.push(m.map_or(Value::Null, |(a, _)| json!(rounded(a / per_area))));
        row.push(m.map_or(Value::Null, |(_, p)| json!(rounded(p / per_length))));
    }
    // Columns without a model label or enum read as before; the area is a key attribute.
    let before = table.columns.len();
    table.labels.resize(before, None);
    table.labels.push(words(&[
        ("cs", "Výměra"),
        ("de", "Fläche"),
        ("en", "Area"),
        ("sk", "Výmera"),
    ]));
    table.labels.push(words(&[
        ("cs", "Obvod"),
        ("de", "Umfang"),
        ("en", "Perimeter"),
        ("sk", "Obvod"),
    ]));
    if !table.enums.is_empty() {
        table.enums.extend([None, None]);
    }
    if !table.summary.is_empty() {
        table.summary.extend([true, false]);
    }
    table
        .columns
        .extend(["area".to_owned(), "perimeter".to_owned()]);
    table
        .units
        .extend([Some(area_unit.to_owned()), Some(length_unit.to_owned())]);
    for geometry in table.geometries.iter_mut().flatten() {
        *geometry = for_display(geometry);
    }
    true
}

/// The OGC API Features item of the one entity `text` holds, on the Endpoint `slug` under
/// `base`: GeoJSON the person can download. `None` when `text` holds no single entity with an id.
pub fn item_link(base: &url::Url, slug: &str, kind: &str, text: &str) -> Option<String> {
    let value: Value = serde_json::from_str(text).ok()?;
    let entities = value.get("entities").unwrap_or(&value);
    let entity = match entities.as_array() {
        Some(list) if list.len() == 1 => list.first()?,
        Some(_) => return None,
        None => entities,
    };
    let id = entity.get("id")?.as_str()?;
    let mut url = base.join("api/endpoint/").ok()?;
    url.path_segments_mut().ok()?.pop_if_empty().extend([
        slug,
        "ogc",
        "features",
        "collections",
        kind,
        "items",
        id,
    ]);
    Some(url.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A register-C parcel of the Slovak cadastre (INSPIRE CP, `849634_1970.C`), read 2026-10-19;
    /// the cadastre states its area as 1,922 m², measured in the national plane projection.
    fn parcel() -> Value {
        json!({"type": "Polygon", "coordinates": [[[19.25650914, 48.20844912], [19.25648289, 48.20841898],
            [19.25447092, 48.20879961], [19.25457032, 48.20889626], [19.25659496, 48.20851327],
            [19.25650914, 48.20844912]]]})
    }

    fn square(lon: f64, lat: f64, side: f64) -> Vec<Value> {
        vec![
            json!([lon, lat]),
            json!([lon + side, lat]),
            json!([lon + side, lat + side]),
            json!([lon, lat + side]),
            json!([lon, lat]),
        ]
    }

    #[test]
    fn a_parcel_measures_as_the_cadastre_states_it_within_a_thousandth() {
        let (area, perimeter) = measure(&parcel()).unwrap_or_default();
        assert!((area - 1922.0).abs() / 1922.0 < 1e-3, "{area}");
        assert!((330.0..350.0).contains(&perimeter), "{perimeter}");
        // the rings wound the other way measure the same
        let mut reversed = parcel();
        if let Some(ring) = reversed
            .pointer_mut("/coordinates/0")
            .and_then(Value::as_array_mut)
        {
            ring.reverse();
        }
        let (again, _) = measure(&reversed).unwrap_or_default();
        assert!((again - area).abs() < 1e-6, "{again} {area}");
    }

    #[test]
    fn a_hole_is_subtracted_and_its_ring_counted_and_a_multipolygon_sums_its_parts() {
        let outer = square(19.0, 48.0, 0.01);
        let hole: Vec<Value> = square(19.0025, 48.0025, 0.005).into_iter().rev().collect();
        let whole = measure(&json!({"type": "Polygon", "coordinates": [outer.clone()]}))
            .unwrap_or_default();
        let holed =
            measure(&json!({"type": "Polygon", "coordinates": [outer.clone(), hole.clone()]}))
                .unwrap_or_default();
        let inner = measure(&json!({"type": "Polygon", "coordinates": [hole]})).unwrap_or_default();
        assert!(
            (holed.0 - (whole.0 - inner.0)).abs() < 1e-3,
            "{holed:?} {whole:?} {inner:?}"
        );
        assert!((holed.1 - (whole.1 + inner.1)).abs() < 1e-6);
        let two = measure(
            &json!({"type": "MultiPolygon", "coordinates": [[outer], [square(19.1, 48.0, 0.01)]]}),
        )
        .unwrap_or_default();
        assert!((two.0 / whole.0 - 2.0).abs() < 1e-3, "{two:?}");
        assert_eq!(
            measure(&json!({"type": "Point", "coordinates": [19.0, 48.0]})),
            None
        );
        assert_eq!(
            measure(&json!({"type": "Polygon", "coordinates": [[[19.0]]]})),
            None
        );
    }

    /// A ring of `n` points on a wobbly circle around (19.15, 48.73), the size of a district.
    fn district(n: u32) -> Value {
        let ring: Vec<Value> = (0..=n)
            .map(|i| {
                let a = f64::from(i % n) / f64::from(n) * std::f64::consts::TAU;
                let r = 0.1 + 0.002 * (a * 40.0).sin();
                json!([19.15 + r * a.cos(), 48.73 + r * a.sin()])
            })
            .collect();
        json!({"type": "Polygon", "coordinates": [ring]})
    }

    #[test]
    fn a_district_is_drawn_with_fewer_points_and_stays_a_valid_polygon() {
        let original = district(4000);
        let shown = for_display(&original);
        let points = |g: &Value| {
            g.pointer("/coordinates/0")
                .and_then(Value::as_array)
                .map_or(0, Vec::len)
        };
        assert!(
            points(&shown) < points(&original) / 4,
            "{} of {}",
            points(&shown),
            points(&original)
        );
        assert_eq!(shown.get("type"), Some(&json!("Polygon")));
        assert!(polygons(&shown).is_some_and(|s| s.is_valid()));
        // the drawn area stays within a percent of the measured one
        let (a, _) = measure(&original).unwrap_or_default();
        let (b, _) = measure(&shown).unwrap_or_default();
        assert!((a - b).abs() / a < 0.01, "{a} {b}");
    }

    #[test]
    fn a_parcel_keeps_its_metres_and_a_point_is_left_alone() {
        assert_eq!(for_display(&parcel()), parcel());
        let point = json!({"type": "Point", "coordinates": [19.0, 48.0]});
        assert_eq!(for_display(&point), point);
        // a sliver the simplifier would collapse is drawn as it is
        let sliver = json!({"type": "Polygon", "coordinates": [[[19.0, 48.0], [19.1, 48.0], [19.1, 48.000001], [19.0, 48.0]]]});
        assert_eq!(for_display(&sliver), sliver);
    }

    fn towns(geometries: Vec<Option<Value>>, modelled: bool) -> Table {
        Table {
            endpoint: "hranice".into(),
            entity_type: "AdministrativeArea".into(),
            columns: vec!["name".into()],
            rows: (0..geometries.len())
                .map(|i| vec![json!(format!("Obec {i}"))])
                .collect(),
            units: vec![None],
            labels: Vec::new(),
            enums: if modelled { vec![None] } else { Vec::new() },
            summary: if modelled { vec![true] } else { Vec::new() },
            total: None,
            dataset: None,
            download: None,
            geojson: None,
            profile: None,
            filter: None,
            map: None,
            geometries,
        }
    }

    #[test]
    fn polygon_rows_read_their_area_and_perimeter_and_a_point_row_reads_empty_cells() {
        let mut table = towns(
            vec![
                Some(district(4000)),
                Some(parcel()),
                Some(json!({"type": "Point", "coordinates": [19.0, 48.0]})),
                None,
            ],
            true,
        );
        assert!(measured(&mut table));
        assert_eq!(table.columns, ["name", "area", "perimeter"]);
        assert_eq!(
            table.units,
            [None, Some("km²".to_owned()), Some("km".to_owned())]
        );
        assert_eq!(
            (table.enums.len(), table.summary.clone()),
            (3, vec![true, true, false])
        );
        assert_eq!(table.labels.first(), Some(&None));
        let sk = |i: usize| {
            table
                .labels
                .get(i)
                .cloned()
                .flatten()
                .and_then(|l| l.get("sk").cloned())
        };
        assert_eq!(
            (sk(1), sk(2)),
            (Some("Výmera".to_owned()), Some("Obvod".to_owned()))
        );
        // the parcel reads 0.00192 km², the point and the row without a location nothing
        let cells: Vec<Vec<Value>> = table
            .rows
            .iter()
            .map(|r| r.iter().skip(1).cloned().collect())
            .collect();
        assert_eq!(cells.get(1).and_then(|c| c.first()), Some(&json!(0.00192)));
        assert_eq!(cells.get(2), Some(&vec![Value::Null, Value::Null]));
        assert_eq!(cells.get(3), Some(&vec![Value::Null, Value::Null]));
        // the district is drawn simpler, the parcel as it is
        let points = |g: Option<&Option<Value>>| {
            g.cloned().flatten().and_then(|g| {
                g.pointer("/coordinates/0")
                    .and_then(Value::as_array)
                    .map(Vec::len)
            })
        };
        assert!(points(table.geometries.first()) < Some(1000));
        assert_eq!(table.geometries.get(1), Some(&Some(parcel())));
    }

    #[test]
    fn a_table_without_a_polygon_is_left_as_it_is() {
        let mut table = towns(
            vec![
                Some(json!({"type": "Point", "coordinates": [19.0, 48.0]})),
                None,
            ],
            false,
        );
        let before = serde_json::to_value(&table).ok();
        assert!(!measured(&mut table));
        assert_eq!(serde_json::to_value(&table).ok(), before);
        // a type without a model reads the two labels after empty ones
        let mut table = towns(vec![Some(parcel())], false);
        assert!(measured(&mut table));
        assert_eq!(
            (table.labels.len(), table.enums.len(), table.summary.len()),
            (3, 0, 0)
        );
        assert_eq!(table.units.get(1), Some(&Some("m²".to_owned())));
        assert_eq!(
            table.rows.first().and_then(|r| r.get(1)),
            Some(&json!(1922.0))
        );
    }

    #[test]
    fn the_geojson_link_is_the_ogc_item_of_the_one_entity_its_id_escaped() {
        let base = url::Url::parse("https://api.dev.joinedcontext.com/").ok();
        let link = |text: &str| {
            base.as_ref()
                .and_then(|b| item_link(b, "s1", "AdministrativeArea", text))
        };
        assert_eq!(
            link(r#"[{"id": "urn:ngsi-ld:AdministrativeArea:bbsk.sk:SK0321508454", "type": "AdministrativeArea"}]"#).as_deref(),
            Some("https://api.dev.joinedcontext.com/api/endpoint/s1/ogc/features/collections/AdministrativeArea/items/urn:ngsi-ld:AdministrativeArea:bbsk.sk:SK0321508454")
        );
        assert_eq!(
            link(r#"{"entities": {"id": "urn:a/../b?c#d"}}"#).as_deref(),
            Some("https://api.dev.joinedcontext.com/api/endpoint/s1/ogc/features/collections/AdministrativeArea/items/urn:a%2F..%2Fb%3Fc%23d")
        );
        assert_eq!(link(r#"[{"id": "urn:a"}, {"id": "urn:b"}]"#), None);
        assert_eq!(link(r#"[{"type": "X"}]"#), None);
        assert_eq!(link("not json"), None);
    }

    #[test]
    fn areas_and_lengths_read_in_the_unit_of_their_largest() {
        assert_eq!(area_unit(1922.0), ("m²", 1.0));
        assert_eq!(area_unit(52_000.0), ("ha", 1e4));
        assert_eq!(area_unit(103_000_000.0), ("km²", 1e6));
        assert_eq!(length_unit(390.0), ("m", 1.0));
        assert_eq!(length_unit(45_000.0), ("km", 1e3));
        assert_eq!(
            [
                rounded(1921.6),
                rounded(5.2345),
                rounded(0.012345),
                rounded(0.0)
            ],
            [1922.0, 5.23, 0.0123, 0.0]
        );
    }
}
