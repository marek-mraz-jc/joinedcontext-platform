//! The WebSocket fan-out of one Endpoint (ADR-N-049, EP-114, T-4133): each client filter is
//! compiled once into the stored form the broker's own matcher reads, and an index answers which
//! filters a change may notify, so only those candidates run `antares-matcher`.
//!
//! The index never decides a match. It narrows the candidates by one term of each filter:
//!
//! - an equality index, (attribute, value) to filters, for a `q` conjunction with an `==` term;
//! - numeric bounds, per attribute, for a `q` conjunction with `>`, `>=`, `<` or `<=` on a number;
//! - a geo grid of 0.01° cells for a `geoQ` with a bounded area;
//! - otherwise the filter's entity types, or every change when its selector names no plain type.
//!
//! Entities and filters here are in the broker's stored (expanded) form. The caller cuts a change
//! to the receiving caller's grant before it gets here (ADR-N-049 §2.7).

pub mod hub;
pub mod socket;

use antares_jsonld::{expand_attr_name, expand_types, Context};
use antares_ql::{geo::GeoQuery, CmpOp, QNode, QValue};
use serde_json::{Map, Value};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// A filter's handle in one Endpoint's [`Index`].
pub type FilterId = u64;

/// The geo grid's cell edge, in degrees (about 1.1 km of latitude, 0.55 km of longitude at 60° N).
const CELL: f64 = 0.01;

/// A filter or an entity geometry that covers more cells than this is not placed on the grid:
/// the filter is matched by its types instead, the entity is a candidate for every geo filter.
const MAX_CELLS: usize = 4096;

/// Metres per degree of latitude.
const METRES_PER_DEGREE: f64 = 111_320.0;

/// The stored members the delta ignores: the broker rewrites them on every write.
const SYSTEM_MEMBERS: &[&str] = &[
    "createdAt",
    "modifiedAt",
    "instanceId",
    "https://uri.etsi.org/ngsi-ld/createdAt",
    "https://uri.etsi.org/ngsi-ld/modifiedAt",
    "https://uri.etsi.org/ngsi-ld/instanceId",
];

/// One client filter: an NGSI-LD Subscription in the broker's stored form.
#[derive(Debug, Clone)]
pub struct Filter {
    sub: Value,
    watched: BTreeSet<String>,
    key: Key,
}

/// The one term a filter is indexed by.
#[derive(Debug, Clone, PartialEq)]
enum Key {
    Any,
    Types(Vec<String>),
    Eq(String, String),
    Lower(String, f64),
    Upper(String, f64),
    Cells(Vec<(i32, i32)>),
}

impl Filter {
    /// The stored Subscription, as `antares-matcher` reads it.
    pub fn subscription(&self) -> &Value {
        &self.sub
    }

    /// Records a notification sent now, which starts the filter's `throttling` window.
    pub fn sent(&mut self, at: chrono::DateTime<chrono::Utc>) {
        let stamp = at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        if let Some(sub) = self.sub.as_object_mut() {
            let notification = sub
                .entry("notification")
                .or_insert_with(|| Value::Object(Map::new()));
            if let Some(notification) = notification.as_object_mut() {
                notification.insert("lastNotification".into(), Value::String(stamp));
            }
        }
    }

    /// Whether a change of these attributes is one this filter notifies: any change when it
    /// watches nothing, else a change of a watched attribute (5.2.12 `watchedAttributes`).
    fn fires_on(&self, changed: &BTreeSet<String>) -> bool {
        !changed.is_empty()
            && (self.watched.is_empty() || self.watched.iter().any(|a| changed.contains(a)))
    }
}

/// Compiles an `x-subscribe` body into a [`Filter`], or names what is wrong with it in the words
/// a `400` ProblemDetails carries.
///
/// The members are validated as Create Subscription validates them (TS 104 175 §5.2.12): a
/// selector or `watchedAttributes`, a `q` that parses, a `geoQ` that parses, an `idPattern` that
/// compiles, a non-negative `throttling`. The server sets `notification.endpoint`, so a body that
/// names one is refused. A `q` with a linked-entity term (`attr{…}`) is refused: the fan-out has
/// no store to follow the link into.
pub fn compile(body: &Value, ctx: &Context) -> Result<Filter, String> {
    let mut sub = body
        .as_object()
        .cloned()
        .ok_or("the body of x-subscribe is a Subscription object")?;
    if sub.get("type").and_then(Value::as_str) != Some("Subscription") {
        return Err("a Subscription has \"type\": \"Subscription\"".into());
    }
    if sub
        .get("notification")
        .and_then(|n| n.get("endpoint"))
        .is_some()
    {
        return Err("notification.endpoint is set by the server on a WebSocket".into());
    }
    let entities = match sub.get("entities") {
        None => None,
        Some(Value::Array(selectors)) if !selectors.is_empty() => {
            Some(stored_selectors(selectors, ctx)?)
        }
        Some(_) => return Err("entities is a non-empty array of selectors".into()),
    };
    let watched = match sub.get("watchedAttributes") {
        None => BTreeSet::new(),
        Some(names) => attribute_names(names, ctx, "watchedAttributes")?,
    };
    if entities.is_none() && watched.is_empty() {
        return Err("a Subscription names entities or watchedAttributes".into());
    }
    if let Some(entities) = &entities {
        sub.insert("entities".into(), Value::Array(entities.clone()));
    }
    if let Some(names) = sub
        .get("notification")
        .and_then(|n| n.get("attributes"))
        .cloned()
    {
        attribute_names(&names, ctx, "notification.attributes")?;
    }
    match sub.get("throttling") {
        None => {}
        Some(t) if t.as_f64().is_some_and(|t| t >= 0.0) => {}
        Some(_) => return Err("throttling is a number of seconds, zero or more".into()),
    }
    let q = match sub.get("q") {
        None => None,
        Some(Value::String(q)) => {
            let q = antares_ql::percent_decode(q.as_bytes());
            let node = antares_ql::parse_q(&q).map_err(|e| format!("q: {e}"))?;
            if node.has_linked_paths() {
                return Err(
                    "q: a linked-entity term (attr{…}) is not served over a WebSocket".into(),
                );
            }
            Some(node)
        }
        Some(_) => return Err("q is a string".into()),
    };
    let geo = match sub.get("geoQ") {
        None => None,
        Some(Value::Object(g)) => Some(
            GeoQuery::from_params(&antares_matcher::geo_params(g))
                .map_err(|e| format!("geoQ: {e}"))?
                .ok_or("geoQ names a georel")?,
        ),
        Some(_) => return Err("geoQ is an object".into()),
    };
    let coerced = sub.contains_key("expandValues") || sub.contains_key("jsonKeys");
    let key = q
        .as_ref()
        .filter(|_| !coerced)
        .and_then(|q| q_key(q, ctx))
        .or_else(|| geo.as_ref().and_then(geo_key))
        .unwrap_or_else(|| types_key(entities.as_deref()));
    Ok(Filter {
        sub: Value::Object(sub),
        watched,
        key,
    })
}

/// The selectors with each plain type name expanded, as the broker stores them; a type
/// expression (`A|B`, `(A;B)`) and `*` stay as written, the matcher reads them with the context.
fn stored_selectors(selectors: &[Value], ctx: &Context) -> Result<Vec<Value>, String> {
    let mut out = Vec::with_capacity(selectors.len());
    for selector in selectors {
        let mut selector = selector
            .as_object()
            .cloned()
            .ok_or("each entities selector is an object")?;
        let ty = selector
            .get("type")
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
            .ok_or("each entities selector names a type")?
            .to_owned();
        if !is_expression(&ty) {
            let iri = expand_types(&Value::String(ty), ctx).map_err(|e| e.to_string())?;
            selector.insert("type".into(), iri.into_iter().next().unwrap_or(Value::Null));
        }
        match selector.get("id") {
            None | Some(Value::String(_)) => {}
            Some(Value::Array(ids)) if ids.iter().all(Value::is_string) => {}
            Some(_) => return Err("a selector's id is a string or an array of strings".into()),
        }
        if let Some(pattern) = selector.get("idPattern") {
            let pattern = pattern.as_str().ok_or("idPattern is a string")?;
            antares_ql::regex::compile(pattern).map_err(|e| format!("idPattern: {e}"))?;
        }
        out.push(Value::Object(selector));
    }
    Ok(out)
}

fn is_expression(ty: &str) -> bool {
    ty == "*" || ty.contains(['|', ',', ';', '('])
}

fn attribute_names(names: &Value, ctx: &Context, member: &str) -> Result<BTreeSet<String>, String> {
    let names = names
        .as_array()
        .filter(|names| !names.is_empty())
        .ok_or_else(|| format!("{member} is a non-empty array of attribute names"))?;
    names
        .iter()
        .map(|name| {
            let name = name
                .as_str()
                .ok_or_else(|| format!("{member} holds attribute names"))?;
            expand_attr_name(name, ctx).map_err(|e| e.to_string())
        })
        .collect()
}

fn types_key(entities: Option<&[Value]>) -> Key {
    let Some(entities) = entities else {
        return Key::Any;
    };
    let mut types = Vec::new();
    for selector in entities {
        match selector.get("type").and_then(Value::as_str) {
            Some(ty) if !is_expression(ty) => types.push(ty.to_owned()),
            _ => return Key::Any,
        }
    }
    Key::Types(types)
}

/// The first indexable term of a conjunction: `attr==scalar`, then a numeric bound.
fn q_key(q: &QNode, ctx: &Context) -> Option<Key> {
    let terms = match q {
        QNode::And(terms) => terms.as_slice(),
        single @ QNode::Cmp { .. } => std::slice::from_ref(single),
        _ => return None,
    };
    let plain = terms.iter().filter_map(|term| match term {
        QNode::Cmp { path, op, value }
            if path.links.is_empty() && path.bracket.is_none() && path.path.len() == 1 =>
        {
            Some((ctx.expand_key(&path.path[0]), *op, value))
        }
        _ => None,
    });
    let mut bound = None;
    for (attr, op, value) in plain {
        match (op, value) {
            (CmpOp::Eq, QValue::Num(_) | QValue::Str(_) | QValue::Bool(_)) => {
                return Some(Key::Eq(attr, canonical_q(value)?));
            }
            (CmpOp::Gt | CmpOp::Ge, QValue::Num(n)) if bound.is_none() => {
                bound = Some(Key::Lower(attr, *n));
            }
            (CmpOp::Lt | CmpOp::Le, QValue::Num(n)) if bound.is_none() => {
                bound = Some(Key::Upper(attr, *n));
            }
            _ => {}
        }
    }
    bound
}

fn canonical_q(value: &QValue) -> Option<String> {
    match value {
        QValue::Num(n) => Some(format!("n:{n}")),
        QValue::Str(s) => Some(format!("s:{s}")),
        QValue::Bool(b) => Some(format!("b:{b}")),
        _ => None,
    }
}

fn canonical(value: &Value) -> Option<String> {
    match value {
        Value::Number(n) => n.as_f64().map(|n| format!("n:{n}")),
        Value::String(s) => Some(format!("s:{s}")),
        Value::Bool(b) => Some(format!("b:{b}")),
        _ => None,
    }
}

/// The cells a bounded `geoQ` area touches; `None` for a relation with no bound (`disjoint`,
/// `near` without `maxDistance`), another GeoProperty than `location`, or an area over
/// [`MAX_CELLS`].
fn geo_key(geo: &GeoQuery) -> Option<Key> {
    use antares_ql::geo::Rel;
    if !(geo.geoproperty.is_empty() || geo.geoproperty == "location") {
        return None;
    }
    let reach = match geo.rel {
        Rel::Near { max: Some(max), .. } => max,
        Rel::Within | Rel::Contains | Rel::Intersects | Rel::Overlaps | Rel::Equals => 0.0,
        Rel::Near { max: None, .. } | Rel::Disjoint => return None,
    };
    let (west, south, east, north) = bounds(&geo.coordinates)?;
    let lat = south.abs().max(north.abs()).min(89.0);
    let dlat = reach / METRES_PER_DEGREE;
    let dlon = reach / (METRES_PER_DEGREE * lat.to_radians().cos());
    cells(west - dlon, south - dlat, east + dlon, north + dlat).map(Key::Cells)
}

/// West, south, east, north of every `[lon, lat]` pair in a GeoJSON coordinates tree.
fn bounds(coordinates: &Value) -> Option<(f64, f64, f64, f64)> {
    fn walk(v: &Value, b: &mut Option<(f64, f64, f64, f64)>) {
        let Some(items) = v.as_array() else { return };
        if let [lon, lat, ..] = items.as_slice() {
            if let (Some(lon), Some(lat)) = (lon.as_f64(), lat.as_f64()) {
                *b = Some(match *b {
                    None => (lon, lat, lon, lat),
                    Some((w, s, e, n)) => (w.min(lon), s.min(lat), e.max(lon), n.max(lat)),
                });
                return;
            }
        }
        for item in items {
            walk(item, b);
        }
    }
    let parsed;
    let coordinates = match coordinates {
        Value::String(text) => {
            parsed = serde_json::from_str::<Value>(text).ok()?;
            &parsed
        }
        other => other,
    };
    let mut b = None;
    walk(coordinates, &mut b);
    b
}

fn cells(west: f64, south: f64, east: f64, north: f64) -> Option<Vec<(i32, i32)>> {
    let cell = |deg: f64| (deg / CELL).floor();
    let (x0, y0, x1, y1) = (cell(west), cell(south), cell(east), cell(north));
    if !(x0.is_finite() && y0.is_finite() && x1.is_finite() && y1.is_finite()) {
        return None;
    }
    if (x1 - x0 + 1.0) * (y1 - y0 + 1.0) > MAX_CELLS as f64 {
        return None;
    }
    // The bounds are finite and the product is at most MAX_CELLS, so each fits an i32.
    #[allow(clippy::cast_possible_truncation)]
    let (x0, y0, x1, y1) = (x0 as i32, y0 as i32, x1 as i32, y1 as i32);
    Some(
        (x0..=x1)
            .flat_map(|x| (y0..=y1).map(move |y| (x, y)))
            .collect(),
    )
}

/// A total order on the numbers a bound holds (JSON has no NaN).
#[derive(Debug, Clone, Copy, PartialEq)]
struct Bound(f64);

impl Eq for Bound {}

impl PartialOrd for Bound {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Bound {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.total_cmp(&other.0)
    }
}

/// One Endpoint's filters and the index over them.
#[derive(Debug, Default)]
pub struct Index {
    filters: HashMap<FilterId, Filter>,
    any: BTreeSet<FilterId>,
    by_type: HashMap<String, BTreeSet<FilterId>>,
    eq: HashMap<(String, String), BTreeSet<FilterId>>,
    lower: HashMap<String, BTreeMap<Bound, BTreeSet<FilterId>>>,
    upper: HashMap<String, BTreeMap<Bound, BTreeSet<FilterId>>>,
    grid: HashMap<(i32, i32), BTreeSet<FilterId>>,
    on_grid: BTreeSet<FilterId>,
}

impl Index {
    /// How many filters it holds.
    pub fn len(&self) -> usize {
        self.filters.len()
    }

    /// Whether it holds none.
    pub fn is_empty(&self) -> bool {
        self.filters.is_empty()
    }

    /// One filter, to mark it [`Filter::sent`].
    pub fn get_mut(&mut self, id: FilterId) -> Option<&mut Filter> {
        self.filters.get_mut(&id)
    }

    /// Adds a filter under `id`, replacing the one held there (`x-update`).
    pub fn insert(&mut self, id: FilterId, filter: Filter) {
        self.remove(id);
        match &filter.key {
            Key::Any => {
                self.any.insert(id);
            }
            Key::Types(types) => {
                for ty in types {
                    self.by_type.entry(ty.clone()).or_default().insert(id);
                }
            }
            Key::Eq(attr, value) => {
                self.eq
                    .entry((attr.clone(), value.clone()))
                    .or_default()
                    .insert(id);
            }
            Key::Lower(attr, n) => {
                self.lower
                    .entry(attr.clone())
                    .or_default()
                    .entry(Bound(*n))
                    .or_default()
                    .insert(id);
            }
            Key::Upper(attr, n) => {
                self.upper
                    .entry(attr.clone())
                    .or_default()
                    .entry(Bound(*n))
                    .or_default()
                    .insert(id);
            }
            Key::Cells(cells) => {
                for cell in cells {
                    self.grid.entry(*cell).or_default().insert(id);
                }
                self.on_grid.insert(id);
            }
        }
        self.filters.insert(id, filter);
    }

    /// Takes a filter out (`x-unsubscribe`, a closed connection).
    pub fn remove(&mut self, id: FilterId) -> Option<Filter> {
        let filter = self.filters.remove(&id)?;
        fn drop_from<K: std::hash::Hash + Eq>(
            map: &mut HashMap<K, BTreeSet<FilterId>>,
            key: &K,
            id: FilterId,
        ) {
            if let Some(ids) = map.get_mut(key) {
                ids.remove(&id);
                if ids.is_empty() {
                    map.remove(key);
                }
            }
        }
        fn drop_bound(
            map: &mut HashMap<String, BTreeMap<Bound, BTreeSet<FilterId>>>,
            attr: &str,
            n: f64,
            id: FilterId,
        ) {
            if let Some(bounds) = map.get_mut(attr) {
                if let Some(ids) = bounds.get_mut(&Bound(n)) {
                    ids.remove(&id);
                    if ids.is_empty() {
                        bounds.remove(&Bound(n));
                    }
                }
                if bounds.is_empty() {
                    map.remove(attr);
                }
            }
        }
        match &filter.key {
            Key::Any => {
                self.any.remove(&id);
            }
            Key::Types(types) => {
                for ty in types {
                    drop_from(&mut self.by_type, ty, id);
                }
            }
            Key::Eq(attr, value) => drop_from(&mut self.eq, &(attr.clone(), value.clone()), id),
            Key::Lower(attr, n) => drop_bound(&mut self.lower, attr, *n, id),
            Key::Upper(attr, n) => drop_bound(&mut self.upper, attr, *n, id),
            Key::Cells(cells) => {
                for cell in cells {
                    drop_from(&mut self.grid, cell, id);
                }
                self.on_grid.remove(&id);
            }
        }
        Some(filter)
    }

    /// The filters a change to `doc` may notify: every filter that could match is among them,
    /// and only these need the full predicate.
    pub fn candidates(&self, doc: &Value, ctx: &Context) -> BTreeSet<FilterId> {
        let mut out = self.any.clone();
        for ty in doc
            .get("type")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            out.extend(self.by_type.get(ty).into_iter().flatten());
        }
        let Some(members) = doc.as_object() else {
            return out;
        };
        for (attr, instances) in members {
            for value in targets(instances, ctx) {
                if let Some(canon) = canonical(&value) {
                    out.extend(self.eq.get(&(attr.clone(), canon)).into_iter().flatten());
                }
                if let Some(n) = value.as_f64() {
                    if let Some(bounds) = self.lower.get(attr) {
                        out.extend(bounds.range(..=Bound(n)).flat_map(|(_, ids)| ids));
                    }
                    if let Some(bounds) = self.upper.get(attr) {
                        out.extend(bounds.range(Bound(n)..).flat_map(|(_, ids)| ids));
                    }
                }
            }
        }
        if !self.on_grid.is_empty() {
            match entity_cells(doc) {
                Some(cells) => {
                    for cell in cells {
                        out.extend(self.grid.get(&cell).into_iter().flatten());
                    }
                }
                None => out.extend(&self.on_grid),
            }
        }
        out
    }

    /// Whether filter `id` notifies a change of `changed` to `doc`, by `antares-matcher`.
    pub fn decides(
        &self,
        id: FilterId,
        doc: &Value,
        changed: &BTreeSet<String>,
        ctx: &Context,
    ) -> bool {
        self.filters.get(&id).is_some_and(|filter| {
            filter.fires_on(changed)
                && antares_matcher::would_notify(&filter.sub, doc, ctx, &|_| None)
        })
    }
}

/// The values a one-segment `q` term compares against for one attribute, read from its
/// instances as `antares-ql` reads them: a Property's value (a typed value's `@value`), a
/// Relationship's object, a VocabProperty's expanded vocab, the members of a list one by one.
fn targets(instances: &Value, ctx: &Context) -> Vec<Value> {
    let mut out = Vec::new();
    let Some(instances) = instances.as_array() else {
        return out;
    };
    for instance in instances.iter().filter_map(Value::as_object) {
        let target = [
            "value",
            "object",
            "vocab",
            "json",
            "valueList",
            "objectList",
        ]
        .iter()
        .find_map(|k| instance.get(*k).map(|v| (*k, v)));
        let Some((kind, value)) = target else {
            continue;
        };
        let value = match kind {
            "value" | "valueList" => untyped(value),
            "vocab" => vocab(value, ctx),
            _ => value.clone(),
        };
        match value {
            Value::Array(items) => out.extend(items),
            scalar => out.push(scalar),
        }
    }
    out
}

fn untyped(value: &Value) -> Value {
    match value {
        Value::Object(o) => o.get("@value").cloned().unwrap_or_else(|| value.clone()),
        Value::Array(items) => Value::Array(items.iter().map(untyped).collect()),
        other => other.clone(),
    }
}

fn vocab(value: &Value, ctx: &Context) -> Value {
    match value {
        Value::String(s) => Value::String(ctx.expand_key(s)),
        Value::Array(items) => Value::Array(items.iter().map(|v| vocab(v, ctx)).collect()),
        other => other.clone(),
    }
}

/// The grid cells of the entity's `location`, or `None` when it covers more than [`MAX_CELLS`]
/// (it is then a candidate for every geo filter).
fn entity_cells(doc: &Value) -> Option<Vec<(i32, i32)>> {
    let mut out = Vec::new();
    for instance in doc
        .get(antares_ql::geo::LOCATION_IRI)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(coordinates) = instance.get("value").and_then(|g| g.get("coordinates")) else {
            continue;
        };
        let Some((w, s, e, n)) = bounds(coordinates) else {
            continue;
        };
        out.extend(cells(w, s, e, n)?);
    }
    Some(out)
}

/// The attributes whose instances differ between the two states, the system members aside; every
/// attribute of an entity seen for the first time.
pub fn changed(previous: Option<&Value>, doc: &Value) -> BTreeSet<String> {
    let empty = Map::new();
    let now = doc.as_object().unwrap_or(&empty);
    let before = previous.and_then(Value::as_object).unwrap_or(&empty);
    let attribute = |name: &String| {
        !matches!(name.as_str(), "id" | "type" | "@context" | "scope")
            && !SYSTEM_MEMBERS.contains(&name.as_str())
    };
    now.keys()
        .chain(before.keys())
        .filter(|name| attribute(name))
        .filter(|name| now.get(*name).map(without_system) != before.get(*name).map(without_system))
        .cloned()
        .collect()
}

fn without_system(value: &Value) -> Value {
    match value {
        Value::Object(o) => Value::Object(
            o.iter()
                .filter(|(k, _)| !SYSTEM_MEMBERS.contains(&k.as_str()))
                .map(|(k, v)| (k.clone(), without_system(v)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(without_system).collect()),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // The index's test oracles: `notified` is what the hub's own loop does, `scanned` the loop
    // the index replaces.
    impl Index {
        /// The filters a change from `previous` to `doc` notifies, decided by `antares-matcher`
        /// on the candidates and by the delta: a filter fires only when an attribute it watches
        /// (any, when it watches none) differs from the previous state.
        fn notified(
            &self,
            doc: &Value,
            previous: Option<&Value>,
            ctx: &Context,
        ) -> Vec<FilterId> {
            let changed = changed(previous, doc);
            if changed.is_empty() {
                return Vec::new();
            }
            self.candidates(doc, ctx)
                .into_iter()
                .filter(|id| self.decides(*id, doc, &changed, ctx))
                .collect()
        }

        /// What [`Self::notified`] answers, by running every filter: the loop the index replaces,
        /// kept as the ground truth the index is tested against.
        fn scanned(
            &self,
            doc: &Value,
            previous: Option<&Value>,
            ctx: &Context,
        ) -> Vec<FilterId> {
            let changed = changed(previous, doc);
            let mut ids: Vec<FilterId> = self
                .filters
                .keys()
                .copied()
                .filter(|id| self.decides(*id, doc, &changed, ctx))
                .collect();
            ids.sort_unstable();
            ids
        }
    }

    fn ctx() -> Context {
        antares_jsonld::core_context()
    }

    fn expand(entity: Value) -> Value {
        antares_jsonld::expand_entity(
            entity.as_object().expect("an entity"),
            &ctx(),
            antares_jsonld::ExpandOpts::default(),
        )
        .expect("a valid entity")
    }

    fn vehicle(n: u64, speed: f64, mode: &str, lon: f64, lat: f64) -> Value {
        expand(json!({
            "id": format!("urn:ngsi-ld:Vehicle:{n}"),
            "type": "Vehicle",
            "speed": {"type": "Property", "value": speed},
            "transportMode": {"type": "Property", "value": mode},
            "location": {"type": "GeoProperty", "value": {"type": "Point", "coordinates": [lon, lat]}}
        }))
    }

    fn filter(body: Value) -> Filter {
        compile(&body, &ctx()).expect("a valid filter")
    }

    fn sub(extra: Value) -> Value {
        let mut body = json!({"type": "Subscription", "entities": [{"type": "Vehicle"}]});
        if let (Some(body), Some(extra)) = (body.as_object_mut(), extra.as_object()) {
            body.extend(extra.clone());
        }
        body
    }

    #[test]
    fn each_kind_of_term_picks_its_index() {
        let vehicle = "https://uri.etsi.org/ngsi-ld/default-context/Vehicle".to_owned();
        let speed = "https://uri.etsi.org/ngsi-ld/default-context/speed".to_owned();
        assert_eq!(filter(sub(json!({}))).key, Key::Types(vec![vehicle]));
        assert_eq!(
            filter(sub(json!({"q": "speed>15;transportMode==\"bus\""}))).key,
            Key::Eq(
                "https://uri.etsi.org/ngsi-ld/default-context/transportMode".into(),
                "s:bus".into()
            )
        );
        assert_eq!(
            filter(sub(json!({"q": "speed>15"}))).key,
            Key::Lower(speed.clone(), 15.0)
        );
        assert_eq!(
            filter(sub(json!({"q": "speed<=3"}))).key,
            Key::Upper(speed, 3.0)
        );
        assert!(matches!(
            filter(sub(json!({"geoQ": {"georel": "near;maxDistance==500", "geometry": "Point", "coordinates": [24.94, 60.17]}}))).key,
            Key::Cells(cells) if !cells.is_empty() && cells.len() < 20
        ));
        // Nothing indexable: an Or, a disjoint area, a type expression, watchedAttributes alone.
        assert!(matches!(
            filter(sub(json!({"q": "speed>15|speed<3"}))).key,
            Key::Types(_)
        ));
        assert!(matches!(
            filter(sub(json!({"geoQ": {"georel": "disjoint", "geometry": "Point", "coordinates": [24.94, 60.17]}}))).key,
            Key::Types(_)
        ));
        assert_eq!(
            filter(json!({"type": "Subscription", "entities": [{"type": "Vehicle|Bus"}]})).key,
            Key::Any
        );
        assert_eq!(
            filter(json!({"type": "Subscription", "watchedAttributes": ["speed"]})).key,
            Key::Any
        );
        // A world-sized area is matched by its types, not by 4,096 cells and more.
        assert!(matches!(
            filter(sub(json!({"geoQ": {"georel": "within", "geometry": "Polygon",
                "coordinates": [[[-170.0, -80.0], [170.0, -80.0], [170.0, 80.0], [-170.0, -80.0]]]}}))).key,
            Key::Types(_)
        ));
    }

    #[test]
    fn a_body_create_subscription_refuses_is_refused() {
        for (body, says) in [
            (json!([]), "Subscription object"),
            (
                json!({"entities": [{"type": "Vehicle"}]}),
                "\"type\": \"Subscription\"",
            ),
            (
                json!({"type": "Subscription"}),
                "entities or watchedAttributes",
            ),
            (
                json!({"type": "Subscription", "entities": []}),
                "non-empty array",
            ),
            (
                json!({"type": "Subscription", "entities": [{"id": "urn:a"}]}),
                "names a type",
            ),
            (sub(json!({"q": "(speed>1"})), "q:"),
            (
                sub(json!({"q": "driver{Person:name}==\"x\""})),
                "linked-entity",
            ),
            (sub(json!({"q": 15})), "q is a string"),
            (
                sub(json!({"geoQ": {"georel": "near;maxDistance==500", "geometry": "Point"}})),
                "geoQ:",
            ),
            (
                sub(json!({"geoQ": {"geometry": "Point", "coordinates": [1, 2]}})),
                "geoQ",
            ),
            (sub(json!({"throttling": -1})), "throttling"),
            (sub(json!({"watchedAttributes": []})), "watchedAttributes"),
            (
                sub(json!({"notification": {"attributes": [3]}})),
                "notification.attributes",
            ),
            (
                sub(json!({"notification": {"endpoint": {"uri": "http://x"}}})),
                "set by the server",
            ),
            (
                json!({"type": "Subscription", "entities": [{"type": "Vehicle", "idPattern": "("}]}),
                "idPattern",
            ),
            (
                json!({"type": "Subscription", "entities": [{"type": "Vehicle", "id": 3}]}),
                "id is a string",
            ),
        ] {
            let refused = compile(&body, &ctx()).expect_err(says);
            assert!(refused.contains(says), "{body}: {refused}");
        }
    }

    #[test]
    fn a_rewrite_of_the_same_value_notifies_nothing_and_a_watched_change_does() {
        let mut index = Index::default();
        index.insert(1, filter(sub(json!({"watchedAttributes": ["speed"]}))));
        index.insert(2, filter(sub(json!({}))));
        let first = vehicle(1, 10.0, "bus", 24.9, 60.1);
        assert_eq!(index.notified(&first, None, &ctx()), [1, 2], "new entity");
        assert!(
            index.notified(&first, Some(&first), &ctx()).is_empty(),
            "same state"
        );
        let moved = vehicle(1, 10.0, "tram", 24.9, 60.1);
        assert_eq!(
            index.notified(&moved, Some(&first), &ctx()),
            [2],
            "speed unchanged"
        );
        let faster = vehicle(1, 11.0, "tram", 24.9, 60.1);
        assert_eq!(index.notified(&faster, Some(&moved), &ctx()), [1, 2]);
    }

    #[test]
    fn an_entity_entering_an_area_notifies_and_one_leaving_it_does_not() {
        let mut index = Index::default();
        index.insert(
            1,
            filter(sub(json!({"geoQ": {"georel": "near;maxDistance==1000", "geometry": "Point", "coordinates": [24.94, 60.17]}}))),
        );
        let outside = vehicle(1, 10.0, "bus", 25.5, 60.5);
        let inside = vehicle(1, 10.0, "bus", 24.941, 60.171);
        assert!(index.notified(&outside, None, &ctx()).is_empty());
        assert_eq!(index.notified(&inside, Some(&outside), &ctx()), [1]);
        assert!(index.notified(&outside, Some(&inside), &ctx()).is_empty());
    }

    #[test]
    fn throttling_holds_a_filter_back_until_its_window_ends() {
        let mut index = Index::default();
        index.insert(1, filter(sub(json!({"throttling": 60}))));
        let a = vehicle(1, 10.0, "bus", 24.9, 60.1);
        let b = vehicle(1, 12.0, "bus", 24.9, 60.1);
        assert_eq!(index.notified(&a, None, &ctx()), [1]);
        index.get_mut(1).expect("held").sent(chrono::Utc::now());
        assert!(index.notified(&b, Some(&a), &ctx()).is_empty());
        index
            .get_mut(1)
            .expect("held")
            .sent(chrono::Utc::now() - chrono::Duration::seconds(61));
        assert_eq!(index.notified(&b, Some(&a), &ctx()), [1]);
    }

    #[test]
    fn a_removed_filter_leaves_no_trace_in_the_index() {
        let mut index = Index::default();
        let bodies = [
            sub(json!({})),
            sub(json!({"q": "transportMode==\"bus\""})),
            sub(json!({"q": "speed>15"})),
            sub(json!({"q": "speed<15"})),
            sub(
                json!({"geoQ": {"georel": "near;maxDistance==500", "geometry": "Point", "coordinates": [24.94, 60.17]}}),
            ),
            json!({"type": "Subscription", "watchedAttributes": ["speed"]}),
        ];
        for (id, body) in (0..).zip(bodies) {
            index.insert(id, filter(body));
        }
        for id in 0..6 {
            assert!(index.remove(id).is_some());
        }
        assert!(index.remove(0).is_none());
        assert!(index.is_empty());
        assert!(index.any.is_empty() && index.by_type.is_empty() && index.eq.is_empty());
        assert!(index.lower.is_empty() && index.upper.is_empty());
        assert!(index.grid.is_empty() && index.on_grid.is_empty());
    }

    /// A small deterministic generator, so a failure names its seed and replays.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
        fn float(&mut self, lo: f64, hi: f64) -> f64 {
            #[allow(clippy::cast_precision_loss)]
            let unit = (self.next() % 1_000_000) as f64 / 1_000_000.0;
            lo + unit * (hi - lo)
        }
    }

    const MODES: [&str; 3] = ["bus", "tram", "metro"];

    fn random_filter(rng: &mut Rng) -> Value {
        let lon = rng.float(24.90, 25.00);
        let lat = rng.float(60.15, 60.20);
        let speed = rng.below(30);
        let mode = MODES[usize::try_from(rng.below(3)).unwrap_or(0)];
        let n = rng.below(20);
        match rng.below(11) {
            0 => sub(json!({})),
            1 => {
                json!({"type": "Subscription", "entities": [{"type": "Vehicle", "idPattern": format!("{n}$")}]})
            }
            2 => sub(json!({"q": format!("speed>{speed}")})),
            3 => sub(json!({"q": format!("speed<={speed}")})),
            4 => sub(json!({"q": format!("transportMode==\"{mode}\"")})),
            5 => sub(json!({"q": format!("speed=={speed}")})),
            6 => sub(json!({"q": format!("speed>{speed};transportMode==\"{mode}\"")})),
            7 => sub(
                json!({"geoQ": {"georel": format!("near;maxDistance=={}", 200 + rng.below(3000)), "geometry": "Point", "coordinates": [lon, lat]}}),
            ),
            8 => sub(json!({"watchedAttributes": ["speed"], "q": format!("speed>={speed}")})),
            9 => sub(json!({"q": format!("speed>{speed}|transportMode==\"{mode}\"")})),
            _ => {
                json!({"type": "Subscription", "entities": [{"type": "Vehicle", "id": format!("urn:ngsi-ld:Vehicle:{n}")}]})
            }
        }
    }

    #[test]
    fn the_index_notifies_exactly_what_running_every_filter_would() {
        let ctx = ctx();
        for seed in [1_u64, 7, 4133] {
            let mut rng = Rng(seed);
            let mut index = Index::default();
            for id in 0..400 {
                index.insert(id, filter(random_filter(&mut rng)));
            }
            let mut table: HashMap<u64, Value> = HashMap::new();
            let mut notified = 0;
            for change in 0..1500 {
                // Remove and re-add now and then, so the index is held to the loop after churn.
                if change % 100 == 0 {
                    let id = rng.below(400);
                    let back = index.remove(id).expect("held");
                    index.insert(id, back);
                }
                let n = rng.below(20);
                #[allow(clippy::cast_precision_loss)]
                let speed = rng.below(30) as f64;
                let mode = MODES[usize::try_from(rng.below(3)).unwrap_or(0)];
                let doc = vehicle(
                    n,
                    speed,
                    mode,
                    rng.float(24.88, 25.02),
                    rng.float(60.14, 60.21),
                );
                let previous = table.get(&n);
                let by_index = index.notified(&doc, previous, &ctx);
                assert_eq!(
                    by_index,
                    index.scanned(&doc, previous, &ctx),
                    "seed {seed}, change {change}"
                );
                notified += by_index.len();
                table.insert(n, doc);
            }
            assert!(
                notified > 1500,
                "seed {seed}: the stream notifies something ({notified})"
            );
        }
    }
}
