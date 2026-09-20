//! Whether a point lies inside a granted area (GW11, GW16).
//!
//! Only what the write guard actually needs: a polygon from the grant's `geoQ`, a point
//! from the entity's own `location`, and the question whether the one contains the other.
//! Anything the parser does not recognise — a geometry that is not a polygon, a location
//! that is not a point, a malformed coordinate list — answers "cannot tell", and the
//! caller turns that into a refusal, because a geo check that cannot decide must not pass.

use serde_json::Value;

/// A closed ring of `(longitude, latitude)` pairs, GeoJSON order.
type Ring = Vec<(f64, f64)>;

/// The outer ring and holes of a polygon, as GeoJSON orders them.
#[derive(Debug, Clone, PartialEq)]
pub struct Polygon {
    outer: Ring,
    holes: Vec<Ring>,
}

impl Polygon {
    /// Whether the point lies inside the polygon and outside all of its holes.
    ///
    /// Ray casting: count the edges a ray from the point crosses. A point exactly on an
    /// edge counts as inside, because a boundary case must not silently reject a write on
    /// a district border.
    pub fn contains(&self, point: (f64, f64)) -> bool {
        if !ring_contains(&self.outer, point) {
            return false;
        }
        !self.holes.iter().any(|hole| ring_contains(hole, point))
    }

    /// Whether this polygon lies entirely inside `other` (GW11, T-0149).
    ///
    /// Every vertex inside, no edge crossing an edge, and no hole of `other` reaching into
    /// this one: for the simple polygons a policy draws — a district, a cadastral area — that
    /// is containment exactly. A polygon that only touches `other`'s boundary counts as
    /// contained, the same way a point on the boundary counts as inside, so a grant drawn on
    /// the district border does not refuse the district.
    ///
    /// The hole leg is what a vertex-and-crossing test alone misses (T-2327). An area drawn
    /// **around** a carve-out has every vertex inside the grant and crosses none of its edges,
    /// because it surrounds the hole rather than cutting it — and `geo::intersect` reads a
    /// `true` here as "the caller asked for less than it was granted" and forwards the
    /// caller's own area with nothing to filter against, so the protected site would be
    /// answered from. A hole that merely touches this polygon's boundary counts as reaching
    /// into it, which refuses a little more than containment strictly requires; on a carve-out
    /// that is the right direction to be wrong in.
    pub fn within(&self, other: &Polygon) -> bool {
        if !self.outer.iter().all(|point| other.contains(*point)) {
            return false;
        }
        let theirs = std::iter::once(&other.outer).chain(other.holes.iter());
        if theirs.into_iter().any(|ring| crosses(&self.outer, ring)) {
            return false;
        }
        !other
            .holes
            .iter()
            .any(|hole| hole.iter().any(|point| ring_contains(&self.outer, *point)))
    }
}

/// Whether any edge of one ring properly crosses any edge of another.
///
/// Touching is not crossing: two rings that share a boundary point, or a vertex, do not
/// cross, and a grant whose area is exactly the caller's must stay contained in it.
fn crosses(one: &[(f64, f64)], other: &[(f64, f64)]) -> bool {
    edges(one).any(|(a, b)| edges(other).any(|(c, d)| segments_cross(a, b, c, d)))
}

fn edges(ring: &[(f64, f64)]) -> impl Iterator<Item = ((f64, f64), (f64, f64))> + '_ {
    (0..ring.len()).map(move |index| (ring[index], ring[(index + 1) % ring.len()]))
}

/// Proper segment intersection: the two segments cross at a point interior to both.
fn segments_cross(a: (f64, f64), b: (f64, f64), c: (f64, f64), d: (f64, f64)) -> bool {
    const EPSILON: f64 = 1e-12;
    let side = |(x1, y1): (f64, f64), (x2, y2): (f64, f64), (x, y): (f64, f64)| {
        let cross = (x2 - x1) * (y - y1) - (y2 - y1) * (x - x1);
        if cross > EPSILON {
            1
        } else if cross < -EPSILON {
            -1
        } else {
            0
        }
    };
    let (d1, d2, d3, d4) = (side(a, b, c), side(a, b, d), side(c, d, a), side(c, d, b));
    // Collinear or touching endpoints are 0 on one side, which is not a proper crossing.
    d1 * d2 < 0 && d3 * d4 < 0
}

/// What the gateway forwards for `geoQ`, and what it has to filter itself (GW11, R14).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Intersected {
    /// The `geoQ` sent to the broker.
    pub geo_q: Option<String>,
    /// Grant geometries the answer is filtered against here, because the broker was given
    /// something wider. An entity must lie inside at least one of them.
    pub grants: Vec<String>,
    /// Every granted geometry, whichever of them the broker was given (T-0807).
    ///
    /// `grants` is about a read, where the broker already narrowed what it sent back; this
    /// is about the grant itself, and a write is decided against the grant. The two differ
    /// exactly when one grant area was forwarded: the read needs no second filter, the
    /// write still may not leave that area.
    pub areas: Vec<String>,
    /// The caller's own geometry, when the broker was given the grant's instead.
    pub caller: Option<String>,
    /// Whether the caller's own area was replaced by a smaller one (R22).
    pub restricted: bool,
}

/// Intersects the caller's `geoQ` with the grants' (T-0149, GW11).
///
/// Never a clipped polygon: the smaller of the two is forwarded when one contains the
/// other, and otherwise the grant is forwarded — so the broker can only ever return the
/// granted area — and the caller's own area is applied here, on the way back. Clipping
/// two polygons into a third would be a geometry the caller never asked for and the grant
/// never drew.
pub fn intersect(caller: Option<&str>, grants: &[&str]) -> Intersected {
    let areas: Vec<String> = grants.iter().map(|grant| (*grant).to_owned()).collect();
    let Some((first, rest)) = grants.split_first() else {
        // Nothing granted narrows space, so the caller's own area stands.
        return Intersected {
            geo_q: caller.map(str::to_owned),
            ..Intersected::default()
        };
    };

    // Several grants are several areas and `geoQ` has no union: the broker gets the
    // caller's area if it sent one, and every grant area is applied here.
    if !rest.is_empty() {
        return Intersected {
            geo_q: caller.map(str::to_owned),
            grants: areas.clone(),
            areas,
            caller: None,
            restricted: true,
        };
    }

    let granted = (*first).to_owned();
    let Some(asked) = caller else {
        return Intersected {
            geo_q: Some(granted),
            areas,
            restricted: true,
            ..Intersected::default()
        };
    };

    match (granted_polygon(asked), granted_polygon(&granted)) {
        // The caller asked for less than it was given: its own area is the narrower query.
        (Some(inner), Some(outer)) if inner.within(&outer) => Intersected {
            geo_q: Some(asked.to_owned()),
            areas,
            ..Intersected::default()
        },
        // They overlap, or the caller's is a shape this parser cannot read. Either way the
        // broker gets the grant and the caller's area is applied here. The grant is
        // applied here too: the broker is not what enforces the policy, and an answer it
        // did not narrow must still come out narrowed (GW11).
        (_, Some(_)) => Intersected {
            geo_q: Some(granted.clone()),
            grants: vec![granted],
            areas,
            caller: Some(asked.to_owned()),
            restricted: true,
        },
        // The grant itself is unreadable: it is still forwarded, and it is also applied
        // here, so an answer the broker did not narrow is narrowed on the way back.
        (_, None) => Intersected {
            geo_q: Some(granted.clone()),
            grants: vec![granted],
            areas,
            caller: Some(asked.to_owned()),
            restricted: true,
        },
    }
}

/// The polygon of an NGSI-LD `geoQ`, or `None` when it is not a `within` polygon.
///
/// A `geoQ` is `georel=within;geometry=Polygon;coordinates=[[[lon,lat],…]]`. A relation
/// other than `within` is not a containment check, so this answers `None` rather than
/// guessing what it means.
pub fn granted_polygon(geo_q: &str) -> Option<Polygon> {
    let mut georel = None;
    let mut geometry = None;
    let mut coordinates = None;

    for part in geo_q.split(';') {
        match part.split_once('=') {
            Some(("georel", value)) => georel = Some(value.trim()),
            Some(("geometry", value)) => geometry = Some(value.trim()),
            Some(("coordinates", value)) => coordinates = Some(value.trim()),
            _ => {}
        }
    }

    if georel? != "within" || !geometry?.eq_ignore_ascii_case("Polygon") {
        return None;
    }
    polygon_from(&serde_json::from_str(coordinates?).ok()?)
}

/// The polygon of a GeoJSON `coordinates` array: an outer ring and any holes.
pub fn polygon_from(coordinates: &Value) -> Option<Polygon> {
    let rings: Vec<Ring> = coordinates
        .as_array()?
        .iter()
        .map(ring_from)
        .collect::<Option<Vec<Ring>>>()?;
    let mut rings = rings.into_iter();
    Some(Polygon {
        outer: rings.next()?,
        holes: rings.collect(),
    })
}

fn ring_from(ring: &Value) -> Option<Ring> {
    ring.as_array()?
        .iter()
        .map(|pair| {
            let pair = pair.as_array()?;
            Some((pair.first()?.as_f64()?, pair.get(1)?.as_f64()?))
        })
        .collect()
}

/// The coordinates of an entity's `location`, or `None` when it has none or it is not a
/// point (GW16).
pub fn entity_point(entity: &Value) -> Option<(f64, f64)> {
    let location = entity.get("location")?;
    // NGSI-LD wraps a GeoProperty value; a plain GeoJSON object is accepted too, because
    // that is what a normalized and a simplified payload look like respectively.
    let geometry = location.get("value").unwrap_or(location);
    if !geometry
        .get("type")?
        .as_str()?
        .eq_ignore_ascii_case("Point")
    {
        return None;
    }
    let coordinates = geometry.get("coordinates")?.as_array()?;
    Some((
        coordinates.first()?.as_f64()?,
        coordinates.get(1)?.as_f64()?,
    ))
}

/// Ray casting with the boundary counted as inside.
fn ring_contains(ring: &[(f64, f64)], (x, y): (f64, f64)) -> bool {
    if ring.len() < 3 {
        return false;
    }

    let mut inside = false;
    for window in 0..ring.len() {
        let (x1, y1) = ring[window];
        let (x2, y2) = ring[(window + 1) % ring.len()];

        if on_segment((x1, y1), (x2, y2), (x, y)) {
            return true;
        }
        // The half-open rule on y keeps a vertex from being counted twice.
        if (y1 > y) != (y2 > y) {
            let crossing = (x2 - x1) * (y - y1) / (y2 - y1) + x1;
            if x < crossing {
                inside = !inside;
            }
        }
    }
    inside
}

/// Whether the point lies on the segment, within the tolerance a coordinate carries.
fn on_segment((x1, y1): (f64, f64), (x2, y2): (f64, f64), (x, y): (f64, f64)) -> bool {
    const EPSILON: f64 = 1e-12;
    let cross = (x2 - x1) * (y - y1) - (y2 - y1) * (x - x1);
    if cross.abs() > EPSILON {
        return false;
    }
    x >= x1.min(x2) - EPSILON
        && x <= x1.max(x2) + EPSILON
        && y >= y1.min(y2) - EPSILON
        && y <= y1.max(y2) + EPSILON
}

/// The areas the gateway applies to the broker's answer itself (GW11, T-0149).
///
/// Built once per response, because parsing a polygon per entity would make a large page
/// quadratic for no gain.
#[derive(Debug, Clone, PartialEq)]
pub struct Areas {
    /// The grant areas; an entity must lie inside at least one.
    grants: Vec<Polygon>,
    /// The caller's own area, when the broker was given the grant's instead.
    caller: Option<Polygon>,
    /// An area nobody could parse. Nothing is admitted then: an area the gateway cannot
    /// read is an area no entity can be shown to be in.
    unreadable: bool,
}

impl Areas {
    /// The areas of one constraint set, or `None` when the broker's answer needs no
    /// filtering at all.
    pub fn of(grants: &[String], caller: Option<&str>) -> Option<Areas> {
        if grants.is_empty() && caller.is_none() {
            return None;
        }
        let parsed: Vec<Option<Polygon>> = grants
            .iter()
            .map(|grant| granted_polygon(grant))
            .chain(std::iter::once(caller.and_then(granted_polygon)))
            .collect();

        Some(Areas {
            unreadable: grants.len() + usize::from(caller.is_some())
                != parsed.iter().flatten().count(),
            caller: caller.and(parsed.last().cloned().flatten()),
            grants: parsed.into_iter().take(grants.len()).flatten().collect(),
        })
    }

    /// Whether the entity survives the areas.
    ///
    /// An entity with no readable point does not: the areas only apply when a grant or the
    /// caller drew one, and an entity that cannot be placed cannot be shown to be inside
    /// it. That is also the answer for a spatial query against a type that carries no
    /// location at all.
    pub fn admits(&self, entity: &serde_json::Value) -> bool {
        if self.unreadable {
            return false;
        }
        let Some(point) = entity_point(entity) else {
            return false;
        };
        if self
            .caller
            .as_ref()
            .is_some_and(|area| !area.contains(point))
        {
            return false;
        }
        self.grants.is_empty() || self.grants.iter().any(|area| area.contains(point))
    }
}
