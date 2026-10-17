//! `endpoint: catalog` on the hub and on every named server: the caller's own catalogue (T-4503,
//! ADR-N-025).
//!
//! An agent's first call names the catalogue and finds a dataset with the ordinary NGSI-LD query;
//! its second names the Endpoint the dataset lists. The public catalogue is broker data the
//! Portal writes (T-4500); this one is built per caller from the Endpoints the request reaches,
//! by the same function from the same DCAT-AP records ([`jc_core::ngsi_catalog`]), so the
//! anonymous caller's is the public one entity for entity, and a caller with grants sees, beside
//! it, the Endpoints its token opens and why. Nothing here reads the broker: the records are
//! built without the schema index, which the catalogue does not list.
//!
//! An Endpoint outside the request's reach gives no entity, no word of `searchText` and no
//! count, because the catalogue is computed from `reach` and nothing else; a named server's
//! reach is already its members, so a connector can only narrow it.

use super::jsonrpc::{answered, error, refused, result};
use crate::app::Gateway;
use crate::handlers::endpoint_surface;
use crate::pdp::evaluator::Subject;
use crate::resolver::Endpoint;
use antares_jsonld::{expand_entity, expand_types, Context, ExpandOpts};
use jc_core::kinds::{Audience, QualityKind};
use jc_core::ngsi_catalog::{self, Place};
use serde_json::{json, Map, Value};
use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, HashMap};
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex, OnceLock};

/// The name a tool call gives the catalogue as its `endpoint`.
pub(crate) const NAME: &str = "catalog";

/// The tools the catalogue answers; every other one is the dataset's own.
pub(crate) const TOOLS: &[&str] = &["query_entities", "get_entity", "list_types"];

/// The arguments `query_entities` takes on the catalogue.
const ARGUMENTS: &[&str] = &[
    "type",
    "id",
    "idPattern",
    "q",
    "georel",
    "geometry",
    "coordinates",
    "geoproperty",
    "attrs",
    "pick",
    "format",
    "count",
    "limit",
    "cursor",
];

/// A page when the caller names no `limit`, as the broker's own default.
const DEFAULT_LIMIT: usize = 20;

/// Catalogues kept between requests; past it the cache starts again empty.
const CACHED: usize = 64;

/// Whether a tool call's `endpoint` names the catalogue.
pub(crate) fn names_catalog(named: &str) -> bool {
    named.trim().eq_ignore_ascii_case(NAME)
}

/// One caller's catalogue: the entities, each beside its expanded form the matcher reads.
struct Catalog {
    /// The Endpoints it was built from, held so that a reload, which builds new ones, is a miss
    /// and no address of them is reused while the entry lives.
    built_from: Vec<Arc<Endpoint>>,
    entities: Vec<(Value, Value)>,
}

fn cache() -> &'static Mutex<HashMap<u64, Arc<Catalog>>> {
    static CACHE: OnceLock<Mutex<HashMap<u64, Arc<Catalog>>>> = OnceLock::new();
    CACHE.get_or_init(Mutex::default)
}

fn core() -> &'static Context {
    static CORE: OnceLock<Context> = OnceLock::new();
    CORE.get_or_init(antares_jsonld::core_context)
}

/// The space of the broker-backed public catalogue itself, which is not a dataset of its own.
fn is_catalog_space(endpoint: &Endpoint) -> bool {
    endpoint.space == NAME || endpoint.space == format!("{}-{NAME}", endpoint.project)
}

/// Why the caller sees an Endpoint it reaches.
fn because(endpoint: &Endpoint, subject: &Subject) -> &'static str {
    match endpoint.audience {
        Audience::Public => "public",
        _ if subject.service_account.is_some() => "grant",
        Audience::Organization => "organization",
        Audience::ProjectList => "project",
    }
}

/// The space's `area` rule, which is where `georel=near` finds its Dataset (DM-87).
pub(crate) fn bbox(endpoint: &Endpoint) -> Option<[f64; 4]> {
    let rules = endpoint.declared_types.as_ref()?;
    rules.quality.iter().find_map(|rule| {
        let corners = rule
            .bbox
            .as_ref()
            .filter(|_| rule.kind == QualityKind::Area)?;
        let mut out = [0.0; 4];
        for (slot, corner) in out.iter_mut().zip(corners) {
            *slot = corner.as_f64()?;
        }
        Some(out)
    })
}

/// The caller's catalogue, from the cache while the same Endpoints reach the same way.
fn of(gateway: &Gateway, reach: &[(Arc<Endpoint>, Subject)]) -> Arc<Catalog> {
    let listed: Vec<(&Arc<Endpoint>, &Subject)> = reach
        .iter()
        .filter(|(endpoint, _)| !is_catalog_space(endpoint))
        .map(|(endpoint, subject)| (endpoint, subject))
        .collect();
    let mut hasher = DefaultHasher::new();
    for (endpoint, subject) in &listed {
        endpoint.slug.hash(&mut hasher);
        because(endpoint, subject).hash(&mut hasher);
    }
    let key = hasher.finish();
    let cached = cache()
        .lock()
        .ok()
        .and_then(|cache| cache.get(&key).cloned());
    if let Some(catalog) = cached {
        let same = catalog.built_from.len() == listed.len()
            && catalog
                .built_from
                .iter()
                .zip(&listed)
                .all(|(held, (endpoint, _))| Arc::ptr_eq(held, endpoint));
        if same {
            return catalog;
        }
    }

    let base = gateway.base_url();
    let mut records = Vec::with_capacity(listed.len());
    let mut places = BTreeMap::new();
    for (endpoint, subject) in &listed {
        let index = json!({ "endpoint": endpoint.slug, "models": [] });
        let space = gateway.resolver.resolve_space(&endpoint.space);
        records.push(endpoint_surface::dataset(
            endpoint,
            space.as_deref(),
            &index,
            base,
        ));
        places.insert(
            endpoint.slug.clone(),
            Place {
                space: endpoint.space.clone(),
                bbox: bbox(endpoint),
                access_level: endpoint.audience.as_str().to_owned(),
                visible_because: because(endpoint, subject).to_owned(),
            },
        );
    }
    let feed = endpoint_surface::feed(records, base, &gateway.org_domain);
    let nodes = feed
        .get("@included")
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice);
    let entities = ngsi_catalog::entities(nodes, &places)
        .into_values()
        .map(|entity| {
            let expanded = entity
                .as_object()
                .and_then(|members| expand_entity(members, core(), ExpandOpts::default()).ok())
                .unwrap_or(Value::Null);
            (entity, expanded)
        })
        .collect();
    let catalog = Arc::new(Catalog {
        built_from: listed
            .iter()
            .map(|(endpoint, _)| Arc::clone(endpoint))
            .collect(),
        entities,
    });
    if let Ok(mut cache) = cache().lock() {
        if cache.len() >= CACHED {
            cache.clear();
        }
        cache.insert(key, Arc::clone(&catalog));
    }
    catalog
}

/// One tool call on the catalogue.
pub(crate) fn call(
    gateway: &Gateway,
    reach: &[(Arc<Endpoint>, Subject)],
    id: Value,
    name: &str,
    arguments: &Map<String, Value>,
) -> Value {
    if !TOOLS.contains(&name) {
        return error(
            id,
            -32602,
            &format!(
                "the catalog answers {}; call `{name}` on the Endpoint a Dataset lists in dataService",
                TOOLS.join(", ")
            ),
        );
    }
    tracing::info!(tool = %name, endpoints = reach.len(), "hub catalog call");
    let catalog = of(gateway, reach);
    let answer = match name {
        "list_types" => Ok(answered(&json!(["DataService", "Dataset"]), "entityTypes")),
        "get_entity" => get(&catalog, arguments),
        _ => query(&catalog, arguments),
    };
    result(id, answer.unwrap_or_else(|why| refused(&why, &Value::Null)))
}

fn get(catalog: &Catalog, arguments: &Map<String, Value>) -> Result<Value, String> {
    let wanted = arguments
        .get("id")
        .and_then(Value::as_str)
        .ok_or("get_entity on the catalog takes the `id` of a Dataset or a DataService")?;
    let key_values = key_values_asked(arguments)?;
    let entity = catalog
        .entities
        .iter()
        .find(|(entity, _)| entity["id"] == wanted)
        .map(|(entity, _)| shaped(entity, key_values, None))
        .ok_or_else(|| format!("the catalog holds no entity `{wanted}`"))?;
    Ok(answered(&entity, "entity"))
}

fn query(catalog: &Catalog, arguments: &Map<String, Value>) -> Result<Value, String> {
    if let Some(other) = arguments
        .keys()
        .find(|key| !ARGUMENTS.contains(&key.as_str()))
    {
        return Err(format!(
            "the catalog does not take `{other}`; it takes {}",
            ARGUMENTS.join(", ")
        ));
    }
    let text = |key: &str| arguments.get(key).and_then(Value::as_str);
    let mut sub = json!({ "type": "Subscription" });
    if let Some(types) = text("type") {
        let types = if types.contains(['|', ',', ';', '(']) || types == "*" {
            Value::String(types.to_owned())
        } else {
            expand_types(&Value::String(types.to_owned()), core())
                .ok()
                .and_then(|iris| iris.into_iter().next())
                .ok_or_else(|| format!("`type` {types} is not a type name"))?
        };
        sub["entities"] = json!([{ "type": types }]);
    }
    if let Some(pattern) = text("idPattern") {
        antares_ql::regex::compile(pattern).map_err(|e| format!("idPattern: {e}"))?;
        let mut selectors = sub["entities"]
            .as_array()
            .cloned()
            .unwrap_or_else(|| vec![json!({})]);
        for selector in &mut selectors {
            selector["idPattern"] = json!(pattern);
        }
        sub["entities"] = Value::Array(selectors);
    }
    if let Some(q) = text("q") {
        let q = folded_search(&antares_ql::percent_decode(q.as_bytes()));
        antares_ql::parse_q(&q).map_err(|e| format!("q: {e}"))?;
        sub["q"] = json!(q);
    }
    if let Some(georel) = text("georel") {
        let mut geo = json!({ "georel": georel });
        for key in ["geometry", "coordinates", "geoproperty"] {
            if let Some(value) = arguments.get(key) {
                geo[key] = value.clone();
            }
        }
        let params = geo
            .as_object()
            .map(antares_matcher::geo_params)
            .unwrap_or_default();
        antares_ql::geo::GeoQuery::from_params(&params).map_err(|e| format!("georel: {e}"))?;
        sub["geoQ"] = geo;
    }
    let ids: Option<Vec<&str>> = match arguments.get("id") {
        None => None,
        Some(Value::String(ids)) => Some(ids.split(',').map(str::trim).collect()),
        Some(Value::Array(ids)) => Some(ids.iter().filter_map(Value::as_str).collect()),
        Some(_) => return Err("`id` is one id or a comma-separated list".into()),
    };
    let limit = match arguments.get("limit") {
        None => DEFAULT_LIMIT,
        Some(limit) => limit
            .as_u64()
            .and_then(|limit| usize::try_from(limit).ok())
            .filter(|limit| (1..=1000).contains(limit))
            .ok_or("`limit` is a number from 1 to 1000")?,
    };
    let offset = match arguments.get("cursor") {
        None => 0,
        Some(cursor) => cursor
            .as_str()
            .and_then(|cursor| cursor.parse::<usize>().ok())
            .or_else(|| cursor.as_u64().and_then(|n| usize::try_from(n).ok()))
            .ok_or("`cursor` is the nextCursor of the previous page, passed back as given")?,
    };
    let key_values = key_values_asked(arguments)?;
    let attrs: Option<Vec<&str>> = text("pick")
        .or_else(|| text("attrs"))
        .map(|names| names.split(',').map(str::trim).collect());

    let matching: Vec<&Value> = catalog
        .entities
        .iter()
        .filter(|(entity, _)| {
            ids.as_ref()
                .is_none_or(|ids| entity["id"].as_str().is_some_and(|id| ids.contains(&id)))
        })
        .filter(|(_, expanded)| antares_matcher::would_notify(&sub, expanded, core(), &|_| None))
        .map(|(entity, _)| entity)
        .collect();
    let total = matching.len();
    let page: Vec<Value> = matching
        .into_iter()
        .skip(offset)
        .take(limit)
        .map(|entity| shaped(entity, key_values, attrs.as_deref()))
        .collect();
    let mut answer = answered(&Value::Array(page.clone()), "entities");
    if let Some(structured) = answer
        .get_mut("structuredContent")
        .and_then(Value::as_object_mut)
    {
        if offset + page.len() < total {
            structured.insert(
                "nextCursor".into(),
                json!((offset + page.len()).to_string()),
            );
        }
        if arguments.get("count").and_then(Value::as_bool) == Some(true) {
            structured.insert("total".into(), json!(total));
        }
    }
    Ok(answer)
}

/// `q` with each `searchText` pattern folded as `searchText` is, so `"Banská"` finds `banska`:
/// an agent types the place as it is written.
fn folded_search(q: &str) -> String {
    static TERM: OnceLock<regex::Regex> = OnceLock::new();
    let term = TERM.get_or_init(|| {
        regex::Regex::new(r#"(searchText!?~=)"([^"]*)""#).expect("a valid pattern")
    });
    term.replace_all(q, |found: &regex::Captures<'_>| {
        // A regex escape (`\D`, `\S`) keeps its case: lowered it would mean its opposite.
        let mut pattern = String::new();
        let mut escaped = false;
        for c in found[2].chars() {
            match escaped {
                true => pattern.push(c),
                false => pattern.push_str(&ngsi_catalog::fold(c.encode_utf8(&mut [0; 4]))),
            }
            escaped = !escaped && c == '\\';
        }
        format!("{}\"{pattern}\"", &found[1])
    })
    .into_owned()
}

/// Whether `format` asks for keyValues; normalized is the default, as on the broker.
fn key_values_asked(arguments: &Map<String, Value>) -> Result<bool, String> {
    match arguments.get("format").and_then(Value::as_str) {
        None | Some("normalized") => Ok(false),
        Some("keyValues" | "simplified") => Ok(true),
        Some(other) => Err(format!(
            "the catalog answers `format` normalized or keyValues, not {other}"
        )),
    }
}

/// The entity as asked: only the attributes named, and keyValues as the broker writes it.
fn shaped(entity: &Value, key_values: bool, attrs: Option<&[&str]>) -> Value {
    let Some(members) = entity.as_object() else {
        return entity.clone();
    };
    let mut out = Map::new();
    for (name, member) in members {
        let core_member = name == "id" || name == "type";
        if !core_member && attrs.is_some_and(|attrs| !attrs.contains(&name.as_str())) {
            continue;
        }
        let value = if key_values && !core_member {
            ["value", "object", "languageMap"]
                .iter()
                .find_map(|key| member.get(*key).cloned())
                .unwrap_or_else(|| member.clone())
        } else {
            member.clone()
        };
        out.insert(name.clone(), value);
    }
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Endpoint {
        Endpoint {
            declared_types: None,
            roles: Default::default(),
            slug: "aaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned(),
            title: BTreeMap::new(),
            description: BTreeMap::new(),
            space: "mesto-ovzdusie".to_owned(),
            project: "mesto".to_owned(),
            audience: Audience::Public,
            allowed_projects: Vec::new(),
            representations: Vec::new(),
            rate_limit: None,
            creates: None,
            file_limits: None,
            hidden_attributes: Default::default(),
            projection: None,
            view_mapping: None,
            catalog: None,
            base_path: String::new(),
            models: Vec::new(),
            policy_names: Vec::new(),
            policies: Vec::new(),
        }
    }

    #[test]
    fn a_workload_sees_an_internal_endpoint_by_its_grant_and_a_person_by_the_audience() {
        let mut endpoint = fixture();
        let person = Subject {
            user: Some("jana".to_owned()),
            ..Subject::anonymous()
        };
        let workload = Subject {
            service_account: Some("conformance".to_owned()),
            ..Subject::anonymous()
        };
        endpoint.audience = Audience::Public;
        assert_eq!(because(&endpoint, &workload), "public");
        endpoint.audience = Audience::Organization;
        assert_eq!(because(&endpoint, &person), "organization");
        assert_eq!(because(&endpoint, &workload), "grant");
        endpoint.audience = Audience::ProjectList;
        assert_eq!(because(&endpoint, &person), "project");
    }

    #[test]
    fn a_search_pattern_is_folded_as_search_text_is_but_its_escapes_are_not() {
        assert_eq!(
            folded_search(r#"searchText~="Banská \D+";name=="Žilina""#),
            r#"searchText~="banska \D+";name=="Žilina""#
        );
        assert_eq!(folded_search("pm10>40"), "pm10>40");
    }

    #[test]
    fn the_public_catalogue_is_not_a_dataset_of_itself() {
        let mut endpoint = fixture();
        endpoint.project = "mesto".to_owned();
        endpoint.space = "mesto-catalog".to_owned();
        assert!(is_catalog_space(&endpoint));
        endpoint.space = "mesto-catalogue".to_owned();
        assert!(!is_catalog_space(&endpoint));
    }
}
