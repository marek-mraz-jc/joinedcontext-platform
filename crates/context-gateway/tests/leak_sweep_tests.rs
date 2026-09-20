//! The leak sweep: every read surface of one Endpoint, asked 105 ways, asserting that none of
//! them serves more than (projection ∩ grants − hidden) per type (T-2135; EP-26, MP-02, TS-19).
//!
//! One Endpoint, a projection whose classes have different slots (`User: [name, age, location]`,
//! `Vehicle: [name, weight, location]`), `secretPin` hidden, a public grant over the space, and a
//! `Depot` the projection never names. The broker answers every request with every attribute of
//! every type, each value a canary `CANARY-{Type}-{attr}` and each schema description
//! `DESC-{Type}-{attr}`, so anything that should have been cut is visible in the bytes.
//!
//! The probes cover NGSI-LD (query, retrieve, temporal, batch query, types and attributes), the
//! file downloads (`json`, `csv`, `geojson`, `xlsx`, `zip` — the two compressed ones are opened
//! and their parts read), OGC API Features, SensorThings, the schema artifacts, the access
//! document, the endpoint record and every read-only MCP tool. Two brokers: one that honours
//! `type` and `attrs`, and one that answers more than it was asked, which is what a federated
//! source, a registration answering for a neighbour or a defect looks like.
//!
//! An answer is judged by what it carries and a request by what it selects on: a filter is a
//! read, so a name outside the endpoint's own vocabulary may not reach the broker inside anything
//! that decides which entities come back. `every_representation_and_tool_has_a_probe` fails when
//! a surface is added without a probe, because an unswept surface is where the next leak lives.
//!
//! First measured on 2026-09-18 with 42 leaking and 24 oracle probes (T-1862 … T-2135 come from
//! that run, and `tasks/audit/leak-probe/` holds the report); green since 2026-09-19.
use axum::body::Body;
use axum::extract::Request;
use axum::http::{Method, StatusCode};
use axum::routing::any;
use axum::Router;
use context_gateway::app::{router, Gateway};
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::{Endpoint, Model};
use jc_core::envelope::ResourceEnvelope;
use jc_core::kinds::{Audience, ModelProjectionSpec, PolicySpec, Representation};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

const SLUG: &str = "3ecozggnnhjlp5miouhia53mr2";
const DOMAIN: &str = "hel.fi";
const VEHICLE: &str = "urn:ngsi-ld:Vehicle:hel.fi:fleet:bus-01";
const USER: &str = "urn:ngsi-ld:User:hel.fi:fleet:anna";
const DEPOT: &str = "urn:ngsi-ld:Depot:hel.fi:fleet:north";

const VIEW: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: ModelProjection
metadata: { name: view, namespace: helsinki }
spec:
  contextSpaceRef: fleet
  dataModelRef: { kind: DataModel, name: fleet, version: "1" }
  classes:
    - name: User
      slots: [name, age, location]
    - name: Vehicle
      slots: [name, weight, location]
"#;

/// Every attribute any entity carries; the value of `attr` on `kind` is `CANARY-kind-attr`.
const ATTRS: &[&str] = &["name", "age", "weight", "odometer", "secretPin"];
fn allowed(kind: &str, attr: &str) -> bool {
    matches!(
        (kind, attr),
        ("User", "name" | "age") | ("Vehicle", "name" | "weight")
    )
}

fn entity(id: &str, kind: &str) -> Value {
    let mut e = json!({
        "id": id, "type": kind,
        "location": { "type": "GeoProperty", "value": { "type": "Point", "coordinates": [24.9, 60.2] } },
    });
    for attr in ATTRS {
        e[attr] = json!({ "type": "Property", "value": format!("CANARY-{kind}-{attr}"),
                          "observedAt": "2026-09-01T00:00:00Z" });
    }
    e
}

type Hops = Arc<Mutex<Vec<String>>>;

async fn broker(honest: bool) -> (String, Hops) {
    let hops: Hops = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::clone(&hops);
    let app = Router::new().fallback(any(move |request: Request| {
        let recorder = Arc::clone(&recorder);
        async move {
            let path = request.uri().path().to_owned();
            let query = request.uri().query().unwrap_or_default().to_owned();
            let body = axum::body::to_bytes(request.into_body(), 1 << 20).await.unwrap_or_default();
            recorder.lock().expect("log").push(format!(
                "{path}?{query} {}", String::from_utf8_lossy(&body)
            ));
            let param = |name: &str| -> Option<Vec<String>> {
                query.split('&').find_map(|pair| pair.strip_prefix(&format!("{name}="))).map(|v| {
                    v.replace("%2C", ",").replace("%3A", ":").split(',').map(str::to_owned).collect()
                })
            };
            let posted: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            let mut types = param("type").unwrap_or_default();
            if let Some(selectors) = posted["entities"].as_array() {
                let named: Vec<String> = selectors.iter().filter_map(|s| s["type"].as_str().map(str::to_owned)).collect();
                if !named.is_empty() { types.retain(|t| named.contains(t)); }
            }
            let attrs = param("attrs");
            let shape = |mut e: Value| -> Value {
                if let (true, Some(attrs)) = (honest, &attrs) {
                    if let Some(members) = e.as_object_mut() {
                        members.retain(|k, _| k == "id" || k == "type" || attrs.contains(k));
                    }
                }
                e
            };
            let listed: Vec<Value> = [(USER, "User"), (VEHICLE, "Vehicle"), (DEPOT, "Depot")].into_iter()
                .filter(|(_, kind)| !honest || types.is_empty() || types.iter().any(|t| t == kind))
                .map(|(id, kind)| shape(entity(id, kind))).collect();
            let all = Value::Array(listed);
            let answer = if path.ends_with("/types") {
                json!({ "id": "urn:ngsi-ld:EntityTypeList:1", "type": "EntityTypeList",
                        "typeList": ["User", "Vehicle", "Depot"] })
            } else if path.ends_with("/attributes") {
                json!({ "id": "urn:ngsi-ld:AttributeList:1", "type": "AttributeList",
                        "attributeList": ATTRS })
            } else if path.contains("/types/") || path.contains("/attributes/") {
                json!({ "id": "urn:x", "type": "Attribute", "attributeName": "odometer",
                        "attributeCount": 3, "typeNames": ["User", "Vehicle", "Depot"],
                        "attributeDetails": [{ "id": "odometer", "type": "Attribute", "attributeName": "odometer" }] })
            } else if path.contains("Depot") {
                shape(entity(DEPOT, "Depot"))
            } else if path.contains("Vehicle:") || path.contains("Vehicle%3A") {
                shape(entity(VEHICLE, "Vehicle"))
            } else if path.contains("User:") || path.contains("User%3A") {
                shape(entity(USER, "User"))
            } else {
                all
            };
            axum::Json(answer)
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a port");
    let address = listener.local_addr().expect("address");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{address}"), hops)
}

fn endpoint() -> Endpoint {
    let projection = ResourceEnvelope::<ModelProjectionSpec>::from_yaml(VIEW).expect("parses");
    let policy: PolicySpec = serde_norway::from_str(
        "contextSpaceRef: fleet\nassigner: did:web:hel.fi\nassignee: { kind: role, id: public }\n\
         operations: [queryEntity, retrieveEntity, queryBatch, retrieveTemporal, queryTemporal, \
         retrieveEntityTypes, retrieveEntityTypeDetails, retrieveEntityTypeInfo, retrieveAttrTypes, \
         retrieveAttrTypeDetails, retrieveAttrTypeInfo]\n",
    ).expect("policy");
    let properties = |kind: &str| {
        let mut p = json!({ "id": { "type": "string" }, "type": { "const": kind } });
        for attr in ATTRS {
            p[attr] = json!({ "type": "string", "description": format!("DESC-{kind}-{attr}") });
        }
        json!({ "type": "object", "properties": p })
    };
    Endpoint {
        slug: SLUG.to_owned(),
        title: Default::default(),
        description: Default::default(),
        space: "fleet".to_owned(),
        project: "helsinki".to_owned(),
        audience: Audience::Public,
        allowed_projects: Vec::new(),
        representations: vec![
            Representation::NgsiLd,
            Representation::Mcp,
            Representation::GeoJson,
            Representation::Csv,
            Representation::Xlsx,
            Representation::Json,
            Representation::Zip,
            Representation::OgcFeatures,
            Representation::Sta,
        ],
        rate_limit: None,
        file_limits: None,
        hidden_attributes: ["secretPin".to_owned()].into_iter().collect(),
        projection: Some(Arc::new(projection.spec)),
        view_mapping: None,
        base_path: format!("/api/endpoint/{SLUG}"),
        models: vec![Model {
            name: "fleet".to_owned(),
            version: "1.0.0".to_owned(),
            major: 1,
            classes: vec!["Vehicle".to_owned(), "User".to_owned(), "Depot".to_owned()],
            json_schema: Some(
                json!({ "$schema": "http://json-schema.org/draft-07/schema#", "$defs": {
                "User": properties("User"), "Vehicle": properties("Vehicle"), "Depot": properties("Depot") } }),
            ),
            context: Some(json!({ "@context": { "@vocab": "https://hel.fi/schema/",
                "odometer": "https://hel.fi/schema/odometer", "secretPin": "https://hel.fi/schema/secretPin",
                "Depot": "https://hel.fi/schema/Depot" } })),
        }],
        policies: vec![policy],
    }
}

struct Probe {
    label: &'static str,
    method: Method,
    uri: String,
    accept: &'static str,
    body: Option<Value>,
}

fn get(label: &'static str, uri: &str) -> Probe {
    Probe {
        label,
        method: Method::GET,
        uri: uri.to_owned(),
        accept: "application/json",
        body: None,
    }
}
fn mcp(label: &'static str, tool: &str, arguments: Value) -> Probe {
    Probe {
        label,
        method: Method::POST,
        uri: "/mcp".to_owned(),
        accept: "application/json, text/event-stream",
        body: Some(json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                           "params": { "name": tool, "arguments": arguments } })),
    }
}

fn probes() -> Vec<Probe> {
    let e = "/ngsi-ld/v1/entities";
    let t = "/ngsi-ld/v1/temporal/entities";
    let mut all = vec![
        get("ld two types", &format!("{e}?type=User,Vehicle")),
        get("ld one type", &format!("{e}?type=Vehicle")),
        get("ld q on the other type's attribute", &format!("{e}?type=User,Vehicle&q=age%3E30")),
        get("ld q on a foreign attribute, one type", &format!("{e}?type=Vehicle&q=age%3E30")),
        get("ld q on an attribute outside the projection", &format!("{e}?type=Vehicle&q=odometer%3E1")),
        get("ld q on a hidden attribute", &format!("{e}?type=Vehicle&q=secretPin%3D%3D%221%22")),
        get("ld q non-existence of a foreign attribute", &format!("{e}?type=Vehicle&q=!age")),
        get("ld q or-branch", &format!("{e}?type=User,Vehicle&q=age%3E30%7Cweight%3E100")),
        get("ld q dotted path", &format!("{e}?type=Vehicle&q=age.observedAt%3E2026-01-01T00:00:00Z")),
        get("ld q expanded iri", &format!("{e}?type=Vehicle&q=https://hel.fi/schema/odometer%3E1")),
        get("ld attrs foreign", &format!("{e}?type=User,Vehicle&attrs=age")),
        get("ld attrs outside", &format!("{e}?type=Vehicle&attrs=odometer")),
        get("ld pick foreign", &format!("{e}?type=User,Vehicle&pick=id,age,odometer")),
        get("ld omit", &format!("{e}?type=User,Vehicle&omit=name")),
        get("ld keyValues", &format!("{e}?type=User,Vehicle&options=keyValues")),
        get("ld concise", &format!("{e}?type=User,Vehicle&options=concise")),
        get("ld sysAttrs", &format!("{e}?type=User,Vehicle&options=sysAttrs")),
        get("ld format simplified", &format!("{e}?type=User,Vehicle&format=simplified")),
        get("ld no type, q", &format!("{e}?q=name~%3D%22B%22")),
        get("ld no type, attrs", &format!("{e}?attrs=name")),
        get("ld type outside the projection", &format!("{e}?type=Depot")),
        get("ld three types, one outside", &format!("{e}?type=User,Vehicle,Depot")),
        get("ld idPattern", &format!("{e}?type=Vehicle&idPattern=.*")),
        get("ld id of another type", &format!("{e}?type=Vehicle&id={DEPOT}")),
        get("ld geoproperty", &format!("{e}?type=User,Vehicle&georel=near;maxDistance==100&geometry=Point&coordinates=[24.9,60.2]&geoproperty=odometer")),
        get("ld join inline", &format!("{e}?type=Vehicle&join=inline&joinLevel=2")),
        get("ld count", &format!("{e}?type=User,Vehicle&count=true&limit=1")),
        get("ld lang", &format!("{e}?type=User,Vehicle&lang=sk")),
        get("ld retrieve vehicle", &format!("{e}/{VEHICLE}")),
        get("ld retrieve vehicle attrs=age", &format!("{e}/{VEHICLE}?attrs=age")),
        get("ld retrieve vehicle keyValues", &format!("{e}/{VEHICLE}?options=keyValues")),
        get("ld retrieve depot", &format!("{e}/{DEPOT}")),
        get("ld temporal two types", &format!("{t}?type=User,Vehicle&timerel=after&timeAt=2026-01-01T00:00:00Z")),
        get("ld temporal attrs foreign", &format!("{t}?type=Vehicle&attrs=age&timerel=after&timeAt=2026-01-01T00:00:00Z")),
        get("ld temporal values", &format!("{t}?type=User,Vehicle&options=temporalValues&timerel=after&timeAt=2026-01-01T00:00:00Z")),
        get("ld temporal aggregated", &format!("{t}?type=Vehicle&attrs=age&aggrMethods=max&aggrPeriodDuration=P1D&options=aggregatedValues&timerel=after&timeAt=2026-01-01T00:00:00Z")),
        get("ld temporal retrieve", &format!("{t}/{VEHICLE}")),
        get("ld temporal retrieve depot", &format!("{t}/{DEPOT}")),
        get("ld types", "/ngsi-ld/v1/types"),
        get("ld types details", "/ngsi-ld/v1/types?details=true"),
        get("ld type info", "/ngsi-ld/v1/types/Vehicle"),
        get("ld type info outside", "/ngsi-ld/v1/types/Depot"),
        get("ld attributes", "/ngsi-ld/v1/attributes"),
        get("ld attributes details", "/ngsi-ld/v1/attributes?details=true"),
        get("ld attribute info outside", "/ngsi-ld/v1/attributes/odometer"),
        get("ld attribute info hidden", "/ngsi-ld/v1/attributes/secretPin"),
        get("file json", "/file.json"),
        get("file json two types", "/file.json?type=User,Vehicle"),
        get("file json q foreign", "/file.json?type=Vehicle&q=age%3E30"),
        get("file csv", "/file.csv?type=User,Vehicle"),
        get("file csv one type", "/file.csv?type=Vehicle"),
        get("file csv attrs foreign", "/file.csv?type=Vehicle&attrs=age,odometer"),
        get("file geojson", "/file.geojson?type=User,Vehicle"),
        get("file xlsx", "/file.xlsx?type=User,Vehicle"),
        get("file zip", "/file.zip?type=User,Vehicle"),
        get("ogc landing", "/ogc/features"),
        get("ogc collections", "/ogc/features/collections"),
        get("ogc collection outside", "/ogc/features/collections/Depot"),
        get("ogc items", "/ogc/features/collections/Vehicle/items"),
        get("ogc items outside", "/ogc/features/collections/Depot/items"),
        get("ogc items properties", "/ogc/features/collections/Vehicle/items?properties=age,odometer"),
        get("ogc items filter", "/ogc/features/collections/Vehicle/items?age=CANARY-Vehicle-age"),
        get("ogc queryables", "/ogc/features/collections/Vehicle/queryables"),
        get("ogc item", &format!("/ogc/features/collections/Vehicle/items/{VEHICLE}")),
        get("sta root", "/sta/v1.1"),
        get("sta things", "/sta/v1.1/Things"),
        get("sta things expand", "/sta/v1.1/Things?$expand=Datastreams"),
        get("sta datastreams", "/sta/v1.1/Datastreams"),
        get("sta observed properties", "/sta/v1.1/ObservedProperties"),
        get("sta observations", "/sta/v1.1/Observations"),
        get("sta filter foreign", "/sta/v1.1/Things?$filter=properties/age%20eq%20%27x%27"),
        get("schema index", "/schema/index.json"),
        get("schema linkml", "/schema/1/model.linkml.yaml"),
        get("schema shacl", "/schema/1/model.shacl.ttl"),
        get("schema owl", "/schema/1/model.owl.ttl"),
        get("schema rdf", "/schema/1/model.rdf.ttl"),
        get("schema json-schema", "/schema/1/model.schema.json"),
        get("schema context", "/schema/1/context.jsonld"),
        get("schema markdown", "/schema/1/model.md"),
        get("access", "/access"),
        get("endpoint record", ""),
        mcp("mcp query one type", "query_entities", json!({ "type": "Vehicle" })),
        mcp("mcp query q foreign", "query_entities", json!({ "type": "Vehicle", "q": "age>30" })),
        mcp("mcp query attrs foreign", "query_entities", json!({ "type": "Vehicle", "attrs": ["age", "odometer"] })),
        mcp("mcp query outside", "query_entities", json!({ "type": "Depot" })),
        mcp("mcp get", "get_entity", json!({ "id": VEHICLE })),
        mcp("mcp get depot", "get_entity", json!({ "id": DEPOT })),
        mcp("mcp batch ids", "batch_query", json!({ "ids": [VEHICLE, USER, DEPOT] })),
        mcp("mcp batch type", "batch_query", json!({ "type": "Vehicle", "q": "odometer>1" })),
        mcp("mcp temporal", "query_temporal", json!({ "type": "Vehicle", "timerel": "after", "timeAt": "2026-01-01T00:00:00Z" })),
        mcp("mcp temporal batch", "batch_query_temporal", json!({ "type": "Vehicle", "timerel": "after", "timeAt": "2026-01-01T00:00:00Z" })),
        mcp("mcp retrieve temporal", "retrieve_temporal", json!({ "id": VEHICLE })),
        mcp("mcp types", "list_types", json!({ "details": true })),
        // The two registry item reads: one name the grant covers and one it does not, because the
        // answer to a type nobody granted must not differ from the answer to a type nobody has.
        mcp("mcp type info", "get_type", json!({ "type": "Vehicle" })),
        mcp("mcp type info outside", "get_type", json!({ "type": "Depot" })),
        mcp("mcp attribute info", "get_attribute", json!({ "attrId": "odometer" })),
        mcp("mcp attribute info outside", "get_attribute", json!({ "attrId": "secretPin" })),
        mcp("mcp attributes", "list_attributes", json!({ "details": true })),
        mcp("mcp schema summary", "describe_schema", json!({})),
        mcp("mcp schema linkml", "describe_schema", json!({ "format": "linkml" })),
        mcp("mcp schema shacl", "describe_schema", json!({ "format": "shacl" })),
        mcp("mcp schema json-schema", "describe_schema", json!({ "format": "json-schema" })),
        mcp("mcp schema outside", "describe_schema", json!({ "format": "linkml", "entityType": "Depot" })),
        mcp("mcp access", "describe_access", json!({})),
    ];
    all.push(Probe {
        label: "ld geo+json",
        method: Method::GET,
        uri: format!("{e}?type=User,Vehicle"),
        accept: "application/geo+json",
        body: None,
    });
    all.push(Probe {
        label: "ld json-ld",
        method: Method::GET,
        uri: format!("{e}?type=User,Vehicle"),
        accept: "application/ld+json",
        body: None,
    });
    all.push(Probe { label: "ld post query", method: Method::POST, uri: "/ngsi-ld/v1/entityOperations/query".to_owned(),
        accept: "application/json",
        body: Some(json!({ "type": "Query", "entities": [{ "type": "User" }, { "type": "Vehicle" }, { "type": "Depot" }],
                           "q": "age>30", "attrs": ["age", "odometer"] })) });
    all.push(Probe {
        label: "ld post query by id",
        method: Method::POST,
        uri: "/ngsi-ld/v1/entityOperations/query".to_owned(),
        accept: "application/json",
        body: Some(json!({ "type": "Query", "entities": [{ "id": DEPOT, "type": "Vehicle" }] })),
    });
    all.push(Probe {
        label: "mcp resources list",
        method: Method::POST,
        uri: "/mcp".to_owned(),
        accept: "application/json, text/event-stream",
        body: Some(json!({ "jsonrpc": "2.0", "id": 1, "method": "resources/list" })),
    });
    all.push(Probe {
        label: "mcp tools list",
        method: Method::POST,
        uri: "/mcp".to_owned(),
        accept: "application/json, text/event-stream",
        body: Some(json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" })),
    });
    all
}

/// What in one answer may not be there: the canary or the schema description of an attribute its
/// own type may not serve, anything of the `Depot` outside the projection, and the name of an
/// attribute this endpoint hides.
///
/// A marker the caller wrote themselves is not a leak: every surface echoes the request back in
/// a `self` link, a `links` array or a problem detail, and a caller learns nothing from reading
/// their own query. `sent` is the whole request, and a marker inside it is not counted.
fn leaks(text: &str, sent: &str) -> Vec<String> {
    let mut found = Vec::new();
    for kind in ["User", "Vehicle", "Depot"] {
        for attr in ATTRS {
            if allowed(kind, attr) {
                continue;
            }
            for marker in [
                format!("CANARY-{kind}-{attr}"),
                format!("DESC-{kind}-{attr}"),
            ] {
                if text.contains(&marker) && !sent.contains(&marker) {
                    found.push(marker);
                }
            }
        }
    }
    for name in ["odometer", "secretPin", "Depot", "north"] {
        if text.contains(name) && !sent.contains(name) {
            found.push(format!("name:{name}"));
        }
    }
    found
}

/// The parameters of a request to the broker that decide *which* entities come back.
///
/// `pick` and `omit` are not among them: CIM 009 clause 4.5.6 makes them a projection of the
/// answer, not a selector of it, so a name in one of them cannot tell a caller which entities
/// exist. `attrs` is a selector — an entity with none of the named attributes is not returned —
/// so it is read here, and the `id` list is not: an id the caller wrote themselves says nothing
/// back to them.
fn selectors(hop: &str) -> String {
    let (path, query) = hop.split_once('?').unwrap_or((hop, ""));
    let (query, body) = query.split_once(' ').unwrap_or((query, ""));
    let carried: Vec<String> = query
        .split('&')
        .filter(|pair| {
            matches!(
                pair.split('=').next().unwrap_or_default(),
                "type"
                    | "q"
                    | "attrs"
                    | "georel"
                    | "geometry"
                    | "coordinates"
                    | "geoproperty"
                    | "orderBy"
                    | "csf"
                    | "scopeQ"
            )
        })
        .map(|pair| {
            pair.replace("%3E", ">")
                .replace("%3D", "=")
                .replace("%21", "!")
        })
        .collect();

    // The body of a batch query carries the same selectors as JSON members. The `id` of an entry
    // is not one of them: an id is the caller's own, they wrote it, and what comes back for it is
    // judged by the answer's type guard rather than by the request (T-2130).
    let posted: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let mut selected: Vec<String> = Vec::new();
    for member in ["q", "attrs", "orderBy", "geoQ", "temporalQ", "scopeQ"] {
        if let Some(value) = posted.get(member) {
            selected.push(format!("{member}={value}"));
        }
    }
    for entry in posted["entities"].as_array().into_iter().flatten() {
        if let Some(named) = entry.get("type").and_then(Value::as_str) {
            selected.push(format!("type={named}"));
        }
    }
    format!("{path} {} {}", carried.join("&"), selected.join("&"))
}

/// Names that reached the broker inside something that selects entities (T-1862, MP-02, R9).
///
/// `odometer` and `secretPin` are served for no type here, so they may never appear in a
/// selector. `age` belongs to `User` alone, so it may appear beside `User` and never beside
/// `Vehicle` on its own. `Depot` is outside the projection, so it may never be asked for as a
/// type at all.
fn oracle(hops: &[String], probe: &Probe) -> Vec<String> {
    let mut found = Vec::new();
    for hop in hops {
        let asked = selectors(hop);
        for name in ["odometer", "secretPin"] {
            if asked.contains(name) {
                found.push(format!(
                    "{}: upstream selects on {name}: {asked}",
                    probe.label
                ));
            }
        }
        let types: Vec<&str> = ["User", "Vehicle", "Depot"]
            .into_iter()
            .filter(|kind| asked.contains(*kind))
            .collect();
        if asked.contains("age") && !types.contains(&"User") {
            found.push(format!(
                "{}: upstream selects on age without User: {asked}",
                probe.label
            ));
        }
        if types.contains(&"Depot") {
            found.push(format!(
                "{}: upstream is asked for a type outside the projection: {asked}",
                probe.label
            ));
        }
    }
    found
}

/// Everything readable in an answer, with a compressed one opened.
///
/// `file.xlsx` and `file.zip` are ZIP containers, and a canary inside a worksheet part is as much
/// a leak as one in a JSON body. The parts are read with the same crate the exporter writes them
/// with, so a leak cannot hide behind the compression (TS-19).
fn readable(headers: &str, bytes: &[u8]) -> String {
    let mut text = format!("{headers}\n{}", String::from_utf8_lossy(bytes));
    if bytes.starts_with(b"PK") {
        let mut archive =
            zip::ZipArchive::new(std::io::Cursor::new(bytes.to_vec())).expect("a ZIP container");
        for index in 0..archive.len() {
            let mut part = archive.by_index(index).expect("a part of the container");
            let name = part.name().to_owned();
            let mut content = Vec::new();
            std::io::Read::read_to_end(&mut part, &mut content).expect("the part reads");
            text.push_str(&format!(
                "\n--- {name}\n{}",
                String::from_utf8_lossy(&content)
            ));
        }
    }
    text
}

/// MP-02, EP-26: no surface of an Endpoint serves more than (projection ∩ grants − hidden), per
/// type, and no request to the broker selects on a name outside it.
#[tokio::test]
async fn no_surface_leaks_with_an_honest_broker() {
    sweep(true).await
}

/// The same, against a broker that answers more than it was asked — a federated source, a
/// registration answering for a neighbour, or a defect. The gateway is the enforcement point and
/// may not depend on any of them (T-2131).
#[tokio::test]
async fn no_surface_leaks_with_a_broker_that_answers_more_than_it_was_asked() {
    sweep(false).await
}

async fn sweep(honest: bool) {
    let mut failures = Vec::new();
    let mut probed = 0;
    for probe in probes() {
        let (upstream, hops) = broker(honest).await;
        let gateway = Arc::new(
            Gateway::new(Broker::new(upstream), Box::new(PolicyPdp), DOMAIN).serve([endpoint()]),
        );
        let mut request = Request::builder()
            .method(probe.method.clone())
            .uri(format!("/api/endpoint/{SLUG}{}", probe.uri))
            .header("accept", probe.accept);
        let body = match &probe.body {
            Some(json) => {
                request = request.header("content-type", "application/json");
                Body::from(json.to_string())
            }
            None => Body::empty(),
        };
        let response = router(gateway)
            .oneshot(request.body(body).expect("a request"))
            .await
            .expect("the gateway answers");
        let status = response.status();
        let headers = format!("{:?}", response.headers());
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("a body");

        let sent = format!(
            "{} {}",
            probe.uri,
            probe
                .body
                .as_ref()
                .map(Value::to_string)
                .unwrap_or_default()
        );
        let mut found = leaks(&readable(&headers, &bytes), &sent);
        found.sort();
        found.dedup();
        if !found.is_empty() {
            failures.push(format!(
                "{} ({status}) served {}",
                probe.label,
                found.join(", ")
            ));
        }
        failures.extend(oracle(&hops.lock().expect("the hop log"), &probe));
        assert!(
            status.is_success() || status.is_client_error(),
            "{} answered {status}, which is the gateway failing rather than refusing",
            probe.label
        );
        probed += 1;
    }
    assert!(probed >= 100, "the sweep stopped after {probed} probes");
    assert!(
        failures.is_empty(),
        "honest broker: {honest}\n{}",
        failures.join("\n")
    );
}

/// TS-19: a representation or an MCP tool added without a probe is not swept, and an unswept
/// surface is where the next leak lives. The match is exhaustive, so a new `Representation`
/// variant does not compile until it is named here and probed.
#[tokio::test]
async fn every_representation_and_tool_has_a_probe() {
    let asked: Vec<String> = probes()
        .iter()
        .map(|probe| {
            format!(
                "{} {}",
                probe.uri,
                probe
                    .body
                    .as_ref()
                    .map(Value::to_string)
                    .unwrap_or_default()
            )
        })
        .collect();
    let swept = |marker: &str| asked.iter().any(|probe| probe.contains(marker));

    for representation in endpoint().representations {
        let path = match representation {
            Representation::NgsiLd => "/ngsi-ld/v1/entities",
            Representation::Mcp => "/mcp",
            Representation::GeoJson => "/file.geojson",
            Representation::Csv => "/file.csv",
            Representation::Xlsx => "/file.xlsx",
            Representation::Json => "/file.json",
            Representation::Zip => "/file.zip",
            Representation::OgcFeatures => "/ogc/features",
            Representation::Sta => "/sta/v1.1",
        };
        assert!(
            swept(path),
            "{} is served and never probed",
            representation.as_str()
        );
    }

    // The tools the endpoint itself offers, asked of the running facade rather than of a list
    // written down here, so a tool added tomorrow is caught by this test today.
    let (upstream, _) = broker(true).await;
    let gateway = Arc::new(
        Gateway::new(Broker::new(upstream), Box::new(PolicyPdp), DOMAIN).serve([endpoint()]),
    );
    let response = router(gateway)
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/api/endpoint/{SLUG}/mcp"))
                .header("content-type", "application/json")
                .header("accept", "application/json, text/event-stream")
                .body(Body::from(
                    json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }).to_string(),
                ))
                .expect("a request"),
        )
        .await
        .expect("the facade answers");
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("a body");
    let listed: Value = serde_json::from_slice(&bytes).expect("the tool list is JSON");
    let tools = listed["result"]["tools"]
        .as_array()
        .expect("the facade lists its tools")
        .clone();
    assert!(!tools.is_empty(), "the facade offered no tool at all");
    for tool in tools {
        // A writing tool creates something and is not a read surface; the sweep is about reads.
        if tool["annotations"]["readOnlyHint"] != json!(true) {
            continue;
        }
        let name = tool["name"].as_str().expect("a tool has a name");
        assert!(
            swept(&format!("\"name\":\"{name}\"")),
            "the tool {name} is offered and never probed"
        );
    }
}

/// R20, EP-26: what the broker says when it fails is the broker's, and it can hold anything it
/// was holding — an entity in a stack trace, a query in an error string. The caller gets the
/// gateway's own problem document.
#[tokio::test]
async fn an_upstream_error_body_never_reaches_the_caller() {
    let failing = Router::new().fallback(any(|| async {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(json!({
                "error": "while reading urn:ngsi-ld:Depot:hel.fi:fleet:north",
                "sample": "CANARY-Depot-secretPin",
                "sql": "select secretPin from entities",
            })),
        )
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a free port");
    let address = listener.local_addr().expect("the bound address");
    tokio::spawn(async move {
        let _ = axum::serve(listener, failing).await;
    });

    let gateway = Arc::new(
        Gateway::new(
            Broker::new(format!("http://{address}")),
            Box::new(PolicyPdp),
            DOMAIN,
        )
        .serve([endpoint()]),
    );
    let response = router(gateway)
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/api/endpoint/{SLUG}/ngsi-ld/v1/entities?type=User,Vehicle"
                ))
                .body(Body::empty())
                .expect("a request"),
        )
        .await
        .expect("the gateway answers");
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("a body");
    let answer = String::from_utf8_lossy(&bytes);
    for held in ["CANARY-Depot-secretPin", "secretPin", "Depot", "select"] {
        assert!(
            !answer.contains(held),
            "the broker's own failure carried {held} to the caller: {answer}"
        );
    }
}

/// EP-26, R24: a grant that names one type, no projection, and a read of another type's entity
/// by id — the read that carries no type of its own, so only the answer can be judged (T-2130).
#[tokio::test]
async fn an_entity_of_another_type_is_not_retrieved_by_id() {
    for uri in [
        format!("/ngsi-ld/v1/entities/{DEPOT}"),
        format!("/ngsi-ld/v1/temporal/entities/{DEPOT}"),
        format!("/ngsi-ld/v1/entities?type=Vehicle&id={DEPOT}"),
    ] {
        let (upstream, _) = broker(true).await;
        let mut one_type = endpoint();
        one_type.projection = None;
        one_type.hidden_attributes = Default::default();
        one_type.policies = vec![serde_norway::from_str(
            "contextSpaceRef: fleet\nassigner: did:web:hel.fi\nassignee: { kind: role, id: public }\n\
             operations: [queryEntity, retrieveEntity, retrieveTemporal]\n\
             information:\n  - entities: [{ type: Vehicle }]\n    propertyNames: [name]\n",
        )
        .expect("the policy spec parses")];
        let gateway = Arc::new(
            Gateway::new(Broker::new(upstream), Box::new(PolicyPdp), DOMAIN).serve([one_type]),
        );
        let response = router(gateway)
            .oneshot(
                Request::builder()
                    .uri(format!("/api/endpoint/{SLUG}{uri}"))
                    .body(Body::empty())
                    .expect("a request"),
            )
            .await
            .expect("the gateway answers");
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("a body");
        let answer = String::from_utf8_lossy(&bytes);
        assert!(
            !answer.contains("CANARY-Depot") && !answer.contains("fleet:north"),
            "{uri} answered {status} with the Depot: {answer}"
        );
    }
}

/// SP-01, EP-26: the same canary space, reached by its own name instead of an Endpoint slug.
///
/// `/cs/{space}/ngsi-ld` and `/cs/{space}/mcp` are a second door to the same decision point, with
/// their own resolver and their own record. A projection that is only applied behind a slug would
/// pass every probe above and leak here, so the NGSI-LD and MCP probes are asked again through it.
#[tokio::test]
async fn no_surface_of_the_space_leaks() {
    let asked: Vec<Probe> = probes()
        .into_iter()
        .filter(|probe| probe.uri.starts_with("/ngsi-ld") || probe.uri.starts_with("/mcp"))
        .collect();
    assert!(
        asked.len() > 40,
        "the space sweep asks {} probes",
        asked.len()
    );

    let mut failures = Vec::new();
    for probe in asked {
        let (upstream, hops) = broker(false).await;
        let published = endpoint();
        let space = context_gateway::resolver::Space {
            endpoint: Arc::new(Endpoint {
                slug: "fleet".to_owned(),
                base_path: "/cs/fleet".to_owned(),
                ..published
            }),
            title: Default::default(),
            description: Default::default(),
            is_sandbox: false,
            default_locale: None,
        };
        let gateway = Arc::new(
            Gateway::new(Broker::new(upstream), Box::new(PolicyPdp), DOMAIN).serve_spaces([space]),
        );
        let mut request = Request::builder()
            .method(probe.method.clone())
            .uri(format!("/cs/fleet{}", probe.uri))
            .header("accept", probe.accept);
        let body = match &probe.body {
            Some(json) => {
                request = request.header("content-type", "application/json");
                Body::from(json.to_string())
            }
            None => Body::empty(),
        };
        let response = router(gateway)
            .oneshot(request.body(body).expect("a request"))
            .await
            .expect("the gateway answers");
        let status = response.status();
        let headers = format!("{:?}", response.headers());
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("a body");
        let sent = format!(
            "{} {}",
            probe.uri,
            probe
                .body
                .as_ref()
                .map(Value::to_string)
                .unwrap_or_default()
        );
        let mut found = leaks(&readable(&headers, &bytes), &sent);
        found.sort();
        found.dedup();
        if !found.is_empty() {
            failures.push(format!(
                "space {} ({status}) served {}",
                probe.label,
                found.join(", ")
            ));
        }
        failures.extend(oracle(&hops.lock().expect("the hop log"), &probe));
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// R9, T-2261: two callers share a URL and are answered differently, so no answer of this gateway
/// may be stored by a cache that serves more than one of them.
///
/// The gateway says so itself rather than relying on the edge to say it: `private` is the answer
/// belonging to one caller, `no-store` is a cache not keeping it at all, and `Vary: Authorization`
/// is what the difference depends on.
#[tokio::test]
async fn a_cached_answer_is_never_served_across_grants() {
    for uri in [
        "/ngsi-ld/v1/entities?type=User,Vehicle",
        "/ngsi-ld/v1/types",
        "/file.csv?type=Vehicle",
        "/schema/index.json",
        "/access",
    ] {
        let (upstream, _) = broker(true).await;
        let gateway = Arc::new(
            Gateway::new(Broker::new(upstream), Box::new(PolicyPdp), DOMAIN).serve([endpoint()]),
        );
        let response = router(gateway)
            .oneshot(
                Request::builder()
                    .uri(format!("/api/endpoint/{SLUG}{uri}"))
                    .body(Body::empty())
                    .expect("a request"),
            )
            .await
            .expect("the gateway answers");
        let headers = response.headers().clone();
        let cache = headers
            .get("cache-control")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();
        assert!(
            cache.contains("private"),
            "{uri} does not say the answer is one caller's: {cache:?}"
        );
        assert!(
            cache.contains("no-store") || cache.contains("no-cache"),
            "{uri} may be stored and replayed: {cache:?}"
        );
        let vary = headers
            .get("vary")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();
        assert!(
            vary.contains("Authorization"),
            "{uri} does not vary by who is asking: {vary:?}"
        );
    }
}
