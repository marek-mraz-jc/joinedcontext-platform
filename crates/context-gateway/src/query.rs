//! The query string, in and out (T-0005, GW2, GW10, GW11).
//!
//! NGSI-LD spreads one geo query over three parameters and one temporal query over three
//! more, while a `Policy` writes each of them as a single string. This module is the
//! translation in both directions, plus the rule that decides which of the caller's own
//! parameters survive: a fixed allow list, so a parameter nobody thought about cannot
//! reach the broker unexamined.

use crate::pdp::evaluator::{Constraints, Request};
use std::collections::BTreeSet;

/// Parameters the caller controls and the gateway forwards unchanged: they shape the
/// answer without widening it.
const PASSTHROUGH: &[&str] = &[
    "aggrMethods",
    "aggrPeriodDuration",
    "containedBy",
    "count",
    "csf",
    "datasetId",
    "details",
    "entityMap",
    "format",
    "id",
    "idPattern",
    "join",
    "joinLevel",
    "lang",
    "lastN",
    "limit",
    "local",
    "offset",
    "omit",
    "options",
    "scopeQ",
    "pick",
    "timeproperty",
    "via",
];

/// The decoded `name=value` pairs of a query string, in the order they were sent.
pub fn parse(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| match pair.split_once('=') {
            Some((name, value)) => (decode(name), decode(value)),
            None => (decode(pair), String::new()),
        })
        .collect()
}

/// The first value of a parameter, if the caller sent one that is not empty.
pub fn first<'a>(params: &'a [(String, String)], name: &str) -> Option<&'a str> {
    params
        .iter()
        .find(|(key, value)| key == name && !value.is_empty())
        .map(|(_, value)| value.as_str())
}

/// Why a query of entities is one the specification refuses, before any grant narrows it
/// (GW31): a `type` that is no NGSI-LD name or type expression, or a `q` whose tokens do not
/// lex. `None` for a well-formed query.
pub fn malformed(params: &[(String, String)]) -> Option<String> {
    // The reason names the parameter, never its value: an agent reads it back (AG-21).
    if let Some(types) = first(params, "type") {
        let named = |c: char| c.is_ascii_alphanumeric() || "_-.:/#,;|()~@%+".contains(c);
        if !types.chars().all(named) {
            return Some("the type is not an NGSI-LD name or type expression".to_owned());
        }
    }
    first(params, "q")
        .filter(|q| !lexes(q))
        .map(|_| "q is not an NGSI-LD query".to_owned())
}

/// Whether a `q` is made of the query language's tokens: outside a double-quoted string no
/// whitespace, no single quote and nothing else the grammar has no use for, every string
/// closed and every parenthesis matched. A lexer, not the broker's parser.
fn lexes(q: &str) -> bool {
    let mut depth = 0usize;
    let mut chars = q.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => loop {
                match chars.next() {
                    Some('"') => break,
                    Some('\\') => {
                        chars.next();
                    }
                    Some(_) => {}
                    None => return false,
                }
            },
            '(' => depth += 1,
            ')' => match depth.checked_sub(1) {
                Some(open) => depth = open,
                None => return false,
            },
            c if c.is_ascii_alphanumeric() || "_-.:/#[]=!<>~;|,+*@%$^&".contains(c) => {}
            _ => return false,
        }
    }
    depth == 0
}

/// What the caller asked for, in the dimensions a grant can narrow (GW11).
pub fn requested(params: &[(String, String)]) -> Request {
    Request {
        types: split_list(first(params, "type")),
        attrs: split_list(first(params, "attrs")),
        q: first(params, "q").map(str::to_owned),
        scope_q: first(params, "scopeQ").map(str::to_owned),
        geo_q: joined(params, &["georel", "geometry", "coordinates"]),
        temporal_q: joined(params, &["timerel", "timeAt", "endTimeAt"]),
        referenced: referenced_attributes(params),
    }
}

/// Every attribute name this request uses to pick or order entities (T-1862, MP-02, R9).
///
/// Owner's rule of 2026-09-18: "if there is a filter in q, geo, scope, etc. on an attribute that
/// is not allowed for that entity, that entity should not be considered at all, because with this
/// you can discover the value just by filtering." The answer was stripped afterwards, so a caller
/// who could not read `age` could still ask `q=age>30` and bisect `N` until the list changed —
/// which reads the value exactly, one bit at a time.
///
/// `pick` and `omit` only shape the answer and are not references. `attrs` is one: in CIM 009 it
/// selects the entities that carry one of the names.
pub fn referenced_attributes(params: &[(String, String)]) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    if let Some(q) = first(params, "q") {
        names.extend(q_attributes(q));
    }
    names.extend(
        split_list(first(params, "attrs"))
            .into_iter()
            .map(|name| compact(&name)),
    );
    for selector in ["orderBy", "geoproperty", "timeproperty"] {
        for name in split_list(first(params, selector)) {
            // `orderBy` takes a leading `!` for descending order; the rest is a path like q's.
            names.insert(head_of(name.trim_start_matches('!')));
        }
    }
    // A geo query with no `geoproperty` is a query on `location`, which CIM 009 makes the default.
    if joined(params, &["georel", "geometry", "coordinates"]).is_some()
        && first(params, "geoproperty").is_none()
    {
        names.insert("location".to_owned());
    }
    names.remove("");
    names
}

/// The same, for a batch query whose selectors travel in the body (CIM 009 clause 5.6.9).
///
/// `POST /entityOperations/query` is the one read whose `q`, `attrs` and geo query are JSON
/// members rather than query parameters, and a filter is a read wherever it is written: a name
/// the endpoint does not serve may no more be selected on here than in a URL (T-2259, T-1862).
pub fn referenced_in_body(body: &serde_json::Value) -> BTreeSet<String> {
    let text = |member: &str| -> Option<String> {
        body.get(member)
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    };
    let mut params: Vec<(String, String)> = Vec::new();
    for member in ["q", "orderBy", "geoproperty", "timeproperty", "georel"] {
        if let Some(value) = text(member) {
            params.push((member.to_owned(), value));
        }
    }
    // `attrs` is a list in the body where the URL spells it comma-separated.
    if let Some(attrs) = body.get("attrs").and_then(serde_json::Value::as_array) {
        let listed: Vec<&str> = attrs.iter().filter_map(serde_json::Value::as_str).collect();
        params.push(("attrs".to_owned(), listed.join(",")));
    } else if let Some(attrs) = text("attrs") {
        params.push(("attrs".to_owned(), attrs));
    }
    // A `geoQ` of its own, which is where a batch query puts the geo selector.
    if let Some(geo) = body.get("geoQ") {
        for member in ["georel", "geoproperty", "geometry", "coordinates"] {
            if let Some(value) = geo.get(member).and_then(serde_json::Value::as_str) {
                params.push((member.to_owned(), value.to_owned()));
            }
        }
    }
    if let Some(temporal) = body.get("temporalQ") {
        if let Some(value) = temporal
            .get("timeproperty")
            .and_then(serde_json::Value::as_str)
        {
            params.push(("timeproperty".to_owned(), value.to_owned()));
        }
    }
    referenced_attributes(&params)
}

/// The attribute names one `q` uses, whatever shape the terms have (CIM 009 4.9).
///
/// Terms are separated by `;`, `|` and parentheses; a term is `path op value`, a bare path
/// (an existence check) or `!path` (a non-existence check). Only the path side is a reference:
/// the value side is the caller's own, and a value that happens to look like a name is not one.
fn q_attributes(q: &str) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for term in q.split([';', '|', '(', ')']) {
        let term = term.trim();
        if term.is_empty() {
            continue;
        }
        // Everything up to the first comparison character is the path; a term with none is a
        // bare existence check and is a path whole.
        let path = term
            .split(['=', '!', '<', '>', '~'])
            .next()
            .unwrap_or_default()
            .trim();
        let path = if path.is_empty() {
            // `!age`: the negation is the first character, so the path follows it.
            term.trim_start_matches('!').trim()
        } else {
            path
        };
        names.insert(head_of(path));
    }
    names.remove("");
    names
}

/// The attribute a path names: its head, compacted.
///
/// `age.observedAt`, `age[value]` and `address[city]` are references to `age`, `age` and
/// `address`; an expanded IRI is the same name as its compacted form, so `https://schema.org/age`
/// is `age`. Case is kept: NGSI-LD attribute names are case sensitive, and folding them would let
/// `Age` stand in for `age`.
fn head_of(path: &str) -> String {
    // An IRI is compacted first: its own path carries the dots and slashes a short name never
    // does, so splitting on `.` before compacting would read `https://schema.org/age` as `https`.
    let compacted = compact(path);
    compacted
        .split(['.', '['])
        .next()
        .unwrap_or_default()
        .trim()
        .to_owned()
}

/// An expanded IRI written as the term it expands to: the last segment of its path.
fn compact(name: &str) -> String {
    let name = decode(name.trim()).trim_matches('"').to_owned();
    match name.rsplit(['/', '#']).next() {
        Some(last) if name.contains("://") && !last.is_empty() => last.to_owned(),
        _ => name,
    }
}

/// The GeoProperty a grant's own area is drawn on.
///
/// A `Policy` writes its spatial constraint as `georel`, `geometry` and `coordinates` and names
/// no property, so the area means what CIM 009 clause 4.10 means by default: `location`.
const GRANTED_GEOPROPERTY: &str = "location";

/// Why the caller's `geoproperty` or `geometryProperty` cannot be served as sent (AG-85, T-2299).
///
/// Both parameters move the attribute a decision is taken on, and the words of a grant were
/// written for the attribute the grant names, so neither can ride through the way a window or a
/// format does. The refusal names the parameter, because a request for a member the caller may
/// not read is a bad request and not an empty answer: an attribute's existence is not the secret
/// here, its value is (CIM 009 clause 5.5.2).
pub fn geo_refusal(params: &[(String, String)], constraints: &Constraints) -> Option<String> {
    if let Some(property) = first(params, "geoproperty") {
        // A grant that draws no area constrains no GeoProperty, so the caller's own pair of
        // property and area goes upstream together and narrows only itself.
        if !constraints.geo_areas.is_empty() && compact(property) != GRANTED_GEOPROPERTY {
            return Some(format!(
                "geoproperty: the area this endpoint grants you is drawn on \
                 `{GRANTED_GEOPROPERTY}`, so a geo query on another GeoProperty cannot be \
                 answered inside it (CIM 009 clause 5.5.2). Send the geo query without \
                 `geoproperty`, or ask for a grant drawn on the property you need"
            ));
        }
    }
    if let Some(property) = first(params, "geometryProperty") {
        let name = compact(property);
        let served = constraints.served.is_empty() || constraints.served.contains(&name);
        if !served || constraints.hidden.contains(&name) {
            return Some(
                "geometryProperty: the GeoJSON geometry is that attribute's own value, and \
                 this endpoint does not serve you the attribute you named. Ask for one of the \
                 attributes the answer already carries"
                    .to_owned(),
            );
        }
    }
    None
}

/// The query string of a write: the caller's harmless parameters and nothing else.
///
/// A write is bounded by the write guard, which reads the payload; narrowing parameters
/// would say nothing about what the body contains and only confuse the broker.
pub fn passthrough(params: &[(String, String)]) -> String {
    render(&kept(params))
}

/// Whether the caller's own query names a selector, as CIM 009 5.7.2 requires of one.
///
/// This asks what the *caller* sent, not what the grants added: a grant narrows the answer to
/// a well-formed query and never supplies the selector a malformed one is missing (GW33). An
/// `id` list or an `idPattern` alone is exactly the case 5.7.2.4 calls `BadRequestData`, so
/// neither counts here; nor does `limit`, `offset` or any other window.
pub fn unselected(params: &[(String, String)]) -> bool {
    let asked = requested(params);
    asked.types.is_empty() && asked.attrs.is_empty() && asked.q.is_none() && asked.geo_q.is_none()
}

/// Whether the query the gateway is about to send selects anything at all.
///
/// CIM 009 5.7.2 refuses a query carrying none of `type`, `attrs`, `q` and `georel`, and a
/// broker that answers one anyway is being more permissive than the specification. None of
/// those four names is in the passthrough allow list, so the caller's own parameters can
/// never supply a selector: after the PDP has spoken, the constraint set is the whole answer.
pub fn selects(constraints: &Constraints) -> bool {
    !constraints.types.is_empty()
        // What reaches the broker as `attrs`: an identity-only set selects nothing there (T-2963).
        || constraints
            .attrs
            .iter()
            .any(|name| !IDENTITY.contains(&name.as_str()))
        || constraints.q.is_some()
        || constraints.geo_q.is_some()
}

/// The query string sent upstream: the caller's harmless parameters, plus the constraint
/// set, which replaces every dimension a grant can narrow (GW2).
///
/// `fallback_types` is the selector of last resort, used only when nothing above selects.
/// A file download passes the types the space holds so that asking for the dataset is itself
/// the selection (EP-09); the NGSI-LD surface passes nothing, because an unselected query
/// there is a `400` and should stay one.
pub fn upstream(
    params: &[(String, String)],
    constraints: &Constraints,
    fallback_types: &[String],
) -> String {
    narrowed(
        params,
        constraints,
        &constraints.attrs,
        fallback_types,
        true,
    )
}

/// [`upstream`] for the temporal evolution of one entity, addressed by its id (T-2987).
///
/// CIM 009 6.19.3.1 takes no `type` there, and a broker that holds to it refuses the request
/// with `400` (Antares does), so every history read under a type grant failed. The grants'
/// types still narrow: what the broker answers is judged by [`crate::pdp::projection::permitted`]
/// before it is served, and an entity of a type no grant names is the gateway's own `404`
/// (T-2130). Without a type beside it, `attrs` is the slots of the type the id names rather than
/// the union over every granted type, so no attribute is asked for beside a type that does not
/// serve it (R9, T-1862). An id that names no type keeps the union.
pub fn upstream_by_id(params: &[(String, String)], constraints: &Constraints, id: &str) -> String {
    let own = crate::relationships::type_of_id(id)
        .and_then(|kind| constraints.attrs_by_type.get(kind))
        .map(|slots| constraints.attrs.intersection(slots).cloned().collect());
    let attrs = own.as_ref().unwrap_or(&constraints.attrs);
    narrowed(params, constraints, attrs, &[], false)
}

fn narrowed(
    params: &[(String, String)],
    constraints: &Constraints,
    attrs: &BTreeSet<String>,
    fallback_types: &[String],
    with_type: bool,
) -> String {
    let mut out = kept(params);

    if with_type && !constraints.types.is_empty() {
        out.push(("type".to_owned(), join_list(&constraints.types)));
    }
    let attrs = broker_attrs(params, attrs);
    if !attrs.is_empty() {
        out.push(("attrs".to_owned(), join_list(&attrs)));
    }
    // The grants' scopes are not a `scopeQ` any more: each policy carries its own, folded
    // into its own `q` term, so no expression can pair one policy's filter with another
    // policy's scope (R12, R13). The caller's own `scopeQ` rides through untouched and the
    // broker ANDs it, which can only narrow.
    if let Some(q) = &constraints.q {
        out.push(("q".to_owned(), q.clone()));
    }
    for compound in [&constraints.geo_q, &constraints.temporal_q]
        .into_iter()
        .flatten()
    {
        out.extend(split_compound(compound));
    }

    // The two parameters that choose an attribute rather than a window (T-2299, AG-85). They
    // are forwarded only after [`geo_refusal`] has passed them: `geoproperty` rides with the
    // geo compound and never alone, because alone it selects nothing and would only offer a
    // grant's own area a second property to be tested against.
    if let Some(property) = first(params, "geoproperty") {
        if constraints.geo_q.is_some() {
            out.push(("geoproperty".to_owned(), property.to_owned()));
        }
    }
    if let Some(property) = first(params, "geometryProperty") {
        out.push(("geometryProperty".to_owned(), property.to_owned()));
    }

    if !selects(constraints) && !fallback_types.is_empty() {
        out.push(("type".to_owned(), fallback_types.join(",")));
    }

    render(&out)
}

/// The caller's own parameters that survive, in the order they were sent.
fn kept(params: &[(String, String)]) -> Vec<(String, String)> {
    params
        .iter()
        .filter(|(name, _)| PASSTHROUGH.contains(&name.as_str()))
        .map(
            |(name, value)| match (name.as_str(), value.parse::<u64>()) {
                // One request cannot make the shared broker read an unbounded history (GW26).
                ("lastN", Ok(n)) if n > LAST_N_CAP => (name.clone(), LAST_N_CAP.to_string()),
                // Nor walk the graph as far as it likes: the same depth the tool schema
                // publishes, and the depth the answer narrowing walks an inlined entity to
                // (AG-25, T-1859).
                ("joinLevel", Ok(n)) if n > JOIN_LEVEL_CAP => {
                    (name.clone(), JOIN_LEVEL_CAP.to_string())
                }
                _ => (name.clone(), value.clone()),
            },
        )
        .collect()
}

/// The most instances per attribute a temporal read asks the broker for (GW26).
pub(crate) const LAST_N_CAP: u64 = 1_000;

/// The deepest chain of linked entities one read walks (AG-25, CIM 009 clause 6.3.11).
///
/// The same bound the MCP tool schema publishes, so the two surfaces cost the broker the
/// same, and the same depth the answer narrowing judges an inlined entity to: a level the
/// gateway would not look at is a level it cannot narrow.
pub(crate) const JOIN_LEVEL_CAP: u64 = 3;

fn render(params: &[(String, String)]) -> String {
    params
        .iter()
        .map(|(name, value)| format!("{}={}", encode(name), encode(value)))
        .collect::<Vec<_>>()
        .join("&")
}

/// Rebuilds a `Policy`'s single-string form from the parameters that carry it on the wire.
fn joined(params: &[(String, String)], names: &[&str]) -> Option<String> {
    let parts: Vec<String> = names
        .iter()
        .filter_map(|name| first(params, name).map(|value| format!("{name}={value}")))
        .collect();
    (!parts.is_empty()).then(|| parts.join(";"))
}

/// The parameters a compound geo or temporal filter is made of, and the only names a `;`
/// may introduce inside one (CIM 009 clauses 4.10 and 4.11).
const COMPOUND_MEMBERS: &[&str] = &[
    "georel",
    "geometry",
    "coordinates",
    "geoproperty",
    "timerel",
    "timeAt",
    "endTimeAt",
    "timeproperty",
];

/// Splits a `Policy`'s single-string form back into the wire parameters.
///
/// A `;` separates parameters only when a parameter name follows it. `georel` carries its
/// own `;` inside its value — `near;maxDistance==2000` is one value, clause 4.10 — and
/// splitting on every `;` sent `maxDistance` upstream as a query parameter of its own,
/// which the broker rightly refused, so no `near` geoquery ever passed the gateway.
fn split_compound(compound: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut rest = compound;
    while !rest.is_empty() {
        let end = rest
            .match_indices(';')
            .map(|(at, _)| at)
            .find(|&at| {
                let after = &rest[at + 1..];
                COMPOUND_MEMBERS.iter().any(|name| {
                    after
                        .strip_prefix(name)
                        .is_some_and(|tail| tail.starts_with('='))
                })
            })
            .unwrap_or(rest.len());
        if let Some((name, value)) = rest[..end].split_once('=') {
            out.push((name.trim().to_owned(), value.trim().to_owned()));
        }
        rest = rest.get(end + 1..).unwrap_or_default();
    }
    out
}

/// An entity's identity, which CIM 009 does not count among its attributes (clause 4.5).
const IDENTITY: [&str; 2] = ["id", "type"];

/// The `attrs` the broker is given (T-2963, R9): the constraint set without `id` and `type`,
/// which the broker refuses as attribute names. A grant names them when an App's
/// `dataNeeds.attrs` does, and `narrow_to_identity` uses them to mean "identity only". The
/// projection keeps them on every entity anyway, and the grant's types still select, so nothing
/// the grant does not cover comes back. A caller whose own `attrs` named one asked the broker
/// that question, and gets its answer.
fn broker_attrs(params: &[(String, String)], attrs: &BTreeSet<String>) -> BTreeSet<String> {
    let asked = split_list(first(params, "attrs"));
    attrs
        .iter()
        .filter(|name| !IDENTITY.contains(&name.as_str()) || asked.contains(*name))
        .cloned()
        .collect()
}

fn split_list(value: Option<&str>) -> BTreeSet<String> {
    value
        .into_iter()
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(str::to_owned)
        .collect()
}

fn join_list(items: &BTreeSet<String>) -> String {
    items.iter().cloned().collect::<Vec<_>>().join(",")
}

/// Percent-encodes everything but the unreserved set of RFC 3986, which is always safe in
/// a query value and never needs a table of exceptions per parameter.
///
/// Public because the egress path carries a whole URL inside one query parameter, which is
/// the same encoding problem this already solves.
pub fn encode(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for byte in raw.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Reverses the percent-encoding, and `+` for the form encoding some clients still send.
///
/// Public because the enforcement point has to decode the entity identifier out of the
/// request path before it can check which organization and space it belongs to.
pub fn decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            // The two digits are read out of the bytes, never out of the string: `i` walks
            // bytes, and slicing the string by a byte index panics when the pair cuts a
            // multi-byte character in half (T-2338).
            b'%' if i + 2 < bytes.len() => {
                match std::str::from_utf8(&bytes[i + 1..i + 3])
                    .map_err(|_| ())
                    .and_then(|pair| u8::from_str_radix(pair, 16).map_err(|_| ()))
                {
                    Ok(byte) => {
                        out.push(byte);
                        i += 3;
                    }
                    // Not an escape after all: a literal `%` is what the caller sent.
                    Err(()) => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_type_or_q_the_specification_refuses_is_named_before_any_grant_narrows_it() {
        let refused = |raw: &str| malformed(&parse(raw));
        assert!(
            refused("idPattern=.*&limit=1").is_none(),
            "well formed, though it selects nothing: that is `unselected`'s refusal, not this one"
        );
        assert!(refused(
            "georel=near%3BmaxDistance%3D%3D100&geometry=Point&coordinates=%5B1%2C2%5D"
        )
        .is_none());

        let hostile = "%27%3B%20DROP%20TABLE%20entities%3B%20--";
        assert!(refused(&format!("type={hostile}"))
            .is_some_and(|why| why.contains("not an NGSI-LD name")));
        assert!(refused(&format!("type=A&q={hostile}"))
            .is_some_and(|why| why.contains("not an NGSI-LD query")));
        assert!(refused(
            "type=(A%3BB)%7CC,https://smartdatamodels.org/dataModel.Env/AirQualityObserved"
        )
        .is_none());

        for q in [
            "pm10>=30",
            "status==%22out%20of%20service%22",
            "(a==1|b!=2);c~=%22x.*%22",
            "dateObserved>=2026-09-01T00:00:00Z",
            "refStation==urn:ngsi-ld:Station:hel.fi:helsinki:001",
            "a.b[c]==1..9",
        ] {
            assert!(refused(&format!("type=A&q={q}")).is_none(), "{q}");
        }
        for q in ["a==1)", "(a==1", "name==%22open", "a%20==%201"] {
            assert!(refused(&format!("type=A&q={q}")).is_some(), "{q}");
        }
    }

    #[test]
    fn a_query_round_trips_through_encoding() {
        let raw = "q=pm10%3E%3D0&type=AirQualityObserved&limit=10";
        let params = parse(raw);
        assert_eq!(first(&params, "q"), Some("pm10>=0"));
        assert_eq!(first(&params, "limit"), Some("10"));
        assert_eq!(encode("pm10>=0"), "pm10%3E%3D0");
        assert_eq!(decode("100%25%20done"), "100% done");
        assert_eq!(decode("a%zz"), "a%zz", "a stray percent is not an escape");
    }

    #[test]
    fn the_three_geo_parameters_are_one_policy_string() {
        let params = parse("georel=within&geometry=Polygon&coordinates=%5B%5B0%2C0%5D%5D&limit=5");
        let request = requested(&params);
        assert_eq!(
            request.geo_q.as_deref(),
            Some("georel=within;geometry=Polygon;coordinates=[[0,0]]")
        );

        let constraints = Constraints {
            geo_q: request.geo_q.clone(),
            ..Constraints::default()
        };
        let upstream = parse(&upstream(&params, &constraints, &[]));
        assert_eq!(first(&upstream, "georel"), Some("within"));
        assert_eq!(first(&upstream, "geometry"), Some("Polygon"));
        assert_eq!(first(&upstream, "coordinates"), Some("[[0,0]]"));
        assert_eq!(
            first(&upstream, "limit"),
            Some("5"),
            "the caller's paging survives"
        );
    }

    /// GW2: the caller's own filters never reach the broker as they were sent; the
    /// constraint set replaces every dimension a grant can narrow.
    ///
    /// `scopeQ` is the exception and it is not a widening: the grants' scopes travel
    /// inside `q` now (R13), and the caller's own `scopeQ` is a filter the broker ANDs on
    /// top, which can only narrow.
    #[test]
    fn a_governed_parameter_is_replaced_and_an_unknown_one_is_dropped() {
        let params =
            parse("q=1%3D1&type=Secret&attrs=everything&scopeQ=%2Fgeo%2FSK&danger=drop%20table");
        let constraints = Constraints {
            types: BTreeSet::from(["AirQualityObserved".to_owned()]),
            attrs: BTreeSet::from(["pm10".to_owned()]),
            q: Some("(1=1);((pm10>=0))".to_owned()),
            granted_scopes: Some("/geo/SK/BB".to_owned()),
            ..Constraints::default()
        };
        let sent = parse(&upstream(&params, &constraints, &[]));

        assert_eq!(first(&sent, "type"), Some("AirQualityObserved"));
        assert_eq!(first(&sent, "attrs"), Some("pm10"));
        assert_eq!(first(&sent, "q"), Some("(1=1);((pm10>=0))"));
        assert_eq!(
            first(&sent, "scopeQ"),
            Some("/geo/SK"),
            "the caller's own scope filter rides through; the grants' are inside q"
        );
        assert_eq!(
            first(&sent, "danger"),
            None,
            "an unlisted parameter is dropped"
        );
    }

    /// T-2987: one entity's history is asked without `type` (CIM 009 6.19.3.1), with the slots of
    /// the type its id names instead of the union, and an id that names no type keeps the union.
    #[test]
    fn a_read_by_id_sends_no_type_and_the_slots_of_its_own_type() {
        let constraints = Constraints {
            types: BTreeSet::from(["User".to_owned(), "Vehicle".to_owned()]),
            attrs: ["age", "location", "name", "weight"]
                .map(str::to_owned)
                .into(),
            attrs_by_type: [
                (
                    "User".to_owned(),
                    ["age", "location", "name"].map(str::to_owned).into(),
                ),
                (
                    "Vehicle".to_owned(),
                    ["location", "name", "weight"].map(str::to_owned).into(),
                ),
            ]
            .into(),
            ..Constraints::default()
        };
        let params = [("lastN".to_owned(), "10".to_owned())];

        let vehicle = parse(&upstream_by_id(
            &params,
            &constraints,
            "urn:ngsi-ld:Vehicle:hel.fi:fleet:bus-01",
        ));
        assert_eq!(first(&vehicle, "type"), None);
        assert_eq!(first(&vehicle, "attrs"), Some("location,name,weight"));
        assert_eq!(first(&vehicle, "lastN"), Some("10"));

        let unnamed = parse(&upstream_by_id(&params, &constraints, "bus-01"));
        assert_eq!(first(&unnamed, "type"), None);
        assert_eq!(first(&unnamed, "attrs"), Some("age,location,name,weight"));

        let query = parse(&upstream(&params, &constraints, &[]));
        assert_eq!(first(&query, "type"), Some("User,Vehicle"));
    }

    /// T-2963, R9: `id` and `type` are an entity's identity, not attributes, and the broker
    /// refuses them in `attrs`. A grant that names them (an App's `dataNeeds.attrs` often does)
    /// or the identity-only set of `narrow_to_identity` reaches the broker without them; the
    /// caller's own `attrs=id` still does, and its answer stays the broker's.
    #[test]
    fn a_grants_identity_members_never_reach_the_broker_as_attributes() {
        let grant = |attrs: &[&str]| Constraints {
            types: BTreeSet::from(["BikeHireDockingStation".to_owned()]),
            attrs: attrs.iter().map(|name| (*name).to_owned()).collect(),
            ..Constraints::default()
        };
        let sent =
            |raw: &str, constraints: &Constraints| parse(&upstream(&parse(raw), constraints, &[]));

        let named = sent("limit=10", &grant(&["id", "name", "availableBikeNumber"]));
        assert_eq!(first(&named, "attrs"), Some("availableBikeNumber,name"));
        assert_eq!(first(&named, "type"), Some("BikeHireDockingStation"));

        // Identity alone: no attrs at all, the grant's types still select, projection keeps id/type.
        for identity in [&["id"][..], &["id", "type"][..]] {
            let only = sent("limit=10", &grant(identity));
            assert_eq!(first(&only, "attrs"), None, "{identity:?}");
            assert_eq!(
                first(&only, "type"),
                Some("BikeHireDockingStation"),
                "{identity:?}"
            );
            let untyped = Constraints {
                types: BTreeSet::new(),
                ..grant(identity)
            };
            assert!(
                !selects(&untyped),
                "an identity-only set is no selector: {identity:?}"
            );
            // A download falls back to the space's types rather than an unselected query.
            let fallback = parse(&upstream(
                &parse("limit=10"),
                &untyped,
                &["Vehicle".to_owned()],
            ));
            assert_eq!(first(&fallback, "type"), Some("Vehicle"), "{identity:?}");
        }

        // The caller asked for `id` in their own words: that question is the broker's to answer.
        let asked = sent("attrs=id&limit=10", &grant(&["id", "name"]));
        assert_eq!(first(&asked, "attrs"), Some("id,name"));
    }

    /// T-0271, CIM 009 clause 4.10: `georel=near;maxDistance==2000` is one value with a `;`
    /// inside it. It has to leave as one `georel` parameter, or the broker refuses the
    /// `maxDistance` it was never meant to see and no `near` query passes the gateway.
    #[test]
    fn a_near_georel_keeps_its_distance_inside_the_value() {
        let params = parse(
            "georel=near%3BmaxDistance%3D%3D2000&geometry=Point&coordinates=%5B19.15%2C48.74%5D",
        );
        assert_eq!(
            requested(&params).geo_q.as_deref(),
            Some("georel=near;maxDistance==2000;geometry=Point;coordinates=[19.15,48.74]")
        );

        let constraints = Constraints {
            geo_q: requested(&params).geo_q,
            ..Constraints::default()
        };
        let sent = parse(&upstream(&params, &constraints, &[]));
        assert_eq!(first(&sent, "georel"), Some("near;maxDistance==2000"));
        assert_eq!(first(&sent, "geometry"), Some("Point"));
        assert_eq!(first(&sent, "coordinates"), Some("[19.15,48.74]"));
        assert_eq!(
            first(&sent, "maxDistance"),
            None,
            "the distance is part of georel, never a parameter of its own"
        );
        assert_eq!(sent.len(), 3);

        // The grant's form is the same string and splits the same way.
        assert_eq!(
            split_compound("georel=within;geometry=Polygon;coordinates=[[[0,0],[1,0],[1,1],[0,0]]];timerel=after;timeAt=P-1D"),
            vec![
                ("georel".to_owned(), "within".to_owned()),
                ("geometry".to_owned(), "Polygon".to_owned()),
                ("coordinates".to_owned(), "[[[0,0],[1,0],[1,1],[0,0]]]".to_owned()),
                ("timerel".to_owned(), "after".to_owned()),
                ("timeAt".to_owned(), "P-1D".to_owned()),
            ]
        );
    }

    /// GW33, CIM 009 5.7.2.4: a query names a selector, and an `id` list or an `idPattern`
    /// alone is not one. The grants do not supply what the caller left out (T-0780).
    #[test]
    fn a_query_naming_no_selector_is_not_rescued_by_the_grants() {
        for raw in [
            "",
            "limit=20",
            "limit=1&offset=40",
            "id=urn:ngsi-ld:AirQualityObserved:hel.fi:ovzdusie:station-1",
            "id=urn:ngsi-ld:Device:hel.fi:s:a,urn:ngsi-ld:Device:hel.fi:s:b&limit=5",
            "idPattern=.*&limit=1",
            "options=keyValues",
        ] {
            assert!(unselected(&parse(raw)), "{raw} names no selector");
        }

        for raw in [
            "type=AirQualityObserved",
            "type=Device&id=urn:ngsi-ld:Device:hel.fi:s:a",
            "attrs=temperature",
            "q=temperature>20",
            "georel=near%3BmaxDistance%3D%3D100&geometry=Point&coordinates=%5B1%2C2%5D",
            "type=Ovzdu%C5%A1ie",
        ] {
            assert!(!unselected(&parse(raw)), "{raw} names a selector");
        }
    }
}
