//! The catalogue of the hub and the named servers (ADR-N-025 2.2, ADR-N-043): each tool's name,
//! the operations that make it worth listing, its description and its published argument
//! schema (T-3648).
//!
//! Discovery stays the gateway's: a tool is listed when the PDP grants one of its operations on
//! an Endpoint of the caller's list, the same PDP that decides every call, so discovery and
//! enforcement cannot drift (SP-15, SP-16). The call itself is the member view's at the broker
//! (T-3808), which checks the arguments again under its own schema; the two describe tools are
//! answered by [`crate::mcp::describe`] and their arguments are checked here first (AG-31).
//! Every schema says `additionalProperties: false`, so an unknown field is an error rather than
//! a parameter that is quietly dropped (AG-21).

use crate::app::Gateway;
use crate::mcp::jsonrpc::refused;
use crate::pdp::evaluator::{Request as PolicyRequest, Subject, Verdict};
use crate::resolver::Endpoint;
use jc_core::kinds::Operation;
use serde_json::{json, Map, Value};
use std::sync::{Arc, LazyLock};

/// An NGSI-LD entity type name, as the models of this platform spell it: a term, never a
/// path, a URL or an escape sequence (AG-21).
///
/// A value outside this is a bad request and is refused by name. Answering it with an empty
/// result would tell the caller the argument was accepted, which is how a traversal probe
/// reads `[]`: as "the parameter went through and there was nothing there".
const TYPE_NAME: &str = "^[A-Za-z][A-Za-z0-9_-]*$";

/// A CIM 009 type selection expression: type names joined by `,` (or), `;` (and) and parentheses.
const TYPE_SELECTION: &str = "^[A-Za-z][A-Za-z0-9_\\-,;|()]*$";

/// An NGSI-LD entity id, which on this platform is always the URN of ADR 001.
const ENTITY_URN: &str = "^urn:ngsi-ld:[^\\s]+$";

/// An NGSI-LD attribute name: a term of the model, or the IRI a term expands to.
///
/// Wider than [`TYPE_NAME`], because an attribute may be sent expanded, and narrow enough that
/// no separator of a query string — `&`, `=`, `;`, `%`, a newline — can ride inside one and
/// arrive at the broker as a second parameter (AG-21, T-2299).
const ATTR_NAME: &str = "^[A-Za-z][A-Za-z0-9_\\-.:/#]*$";

/// One tool of the catalogue (Architecture/07 section 2).
pub(crate) struct Tool {
    pub(crate) name: &'static str,
    /// The operations that make this tool worth advertising: the caller holding any one of
    /// them sees it. Empty is a tool that describes the endpoint rather than its data, and
    /// that every caller the endpoint admits may call (EP-55).
    pub(crate) operations: &'static [Operation],
    pub(crate) description: &'static str,
    pub(crate) schema: fn() -> Value,
    /// What the broker's answer is called in the structured half of the result. MCP's
    /// `structuredContent` is an object matching the tool's output schema, so a bare list
    /// is read as nothing by every client that reads it by name (T-0946, AG-13).
    pub(crate) result_key: &'static str,
}

impl Tool {
    /// Whether the tool only reads, from the operations it stands for rather than from a
    /// boolean somebody set beside them (T-1066, AG-07).
    ///
    /// This is what an agent runtime reads before it decides whether a person has to confirm
    /// the call, so it has to state what the tool does. A tool that writes at all is not
    /// read-only: *any* write is a write, and a rule that asked whether *any* operation reads
    /// would announce a mixed tool as a read.
    fn read_only(&self) -> bool {
        !self.operations.iter().any(Operation::is_write)
    }
}

/// The words `describe_access` and the `access://` resource answer in (EP-56, EP-57, EP-58).
pub(crate) const ACCESS_FORMATS: [&str; 3] = ["permissions", "odrl", "grant-ast"];

pub(crate) const TOOLS: &[Tool] = &[
    Tool {
        name: "query_entities",
        operations: &[Operation::QueryEntity],
        description: "Query the entities of this context space by type and NGSI-LD filter.",
        schema: || {
            // Every selector and representation argument of the shared read parameter table
            // (AG-84): the same names the REST surface takes, from the table the query string is
            // built from, so the two cannot drift.
            json!({
                "type": "object",
                "properties": properties_of(&[SELECTOR_ARGS, REPRESENTATION_ARGS]),
                // CIM 009 5.7.2.4: a query names at least one selector, and `idPattern` is never
                // one on its own. Which of them is present is the caller's business, so the schema
                // asks for one of the five rather than for `type` alone (AG-84).
                "anyOf": [
                    { "required": ["type"] },
                    { "required": ["id"] },
                    { "required": ["attrs"] },
                    { "required": ["q"] },
                    { "required": ["georel"] },
                ],
                "additionalProperties": false,
            })
        },
        result_key: "entities",
    },
    Tool {
        name: "get_entity",
        operations: &[Operation::RetrieveEntity],
        description: "Retrieve one entity of this context space by its exact URN.",
        schema: || {
            // One entity by URN, and every way of narrowing and writing it that the REST path
            // takes (AG-84).
            let mut properties = properties_of(&[PROJECTION_ARGS, REPRESENTATION_ARGS]);
            properties.insert("id".to_owned(), id_schema());
            json!({
                "type": "object",
                "properties": properties,
                "required": ["id"],
                "additionalProperties": false,
            })
        },
        result_key: "entity",
    },
    Tool {
        name: "list_types",
        operations: &[
            Operation::RetrieveEntityTypes,
            Operation::RetrieveEntityTypeDetails,
        ],
        description: "The entity types this context space holds, as the caller may see them.",
        schema: || {
            json!({
                "type": "object",
                "properties": {
                    "details": { "type": "boolean", "description": "Include each type's attribute names" },
                },
                "additionalProperties": false,
            })
        },
        result_key: "entityTypes",
    },
    Tool {
        name: "list_attributes",
        operations: &[
            Operation::RetrieveAttrTypes,
            Operation::RetrieveAttrTypeDetails,
        ],
        description: "The attributes this context space holds, as the caller may see them.",
        schema: || {
            json!({
                "type": "object",
                "properties": {
                    "details": { "type": "boolean", "description": "Include each attribute's types and value kinds" },
                },
                "additionalProperties": false,
            })
        },
        result_key: "attributes",
    },
    Tool {
        name: "query_temporal",
        operations: &[Operation::QueryTemporal],
        description: "Query the history of this context space's entities in a time window.",
        schema: temporal_schema,
        result_key: "entities",
    },
    Tool {
        name: "retrieve_temporal",
        operations: &[Operation::RetrieveTemporal],
        description: "The history of one entity of this context space in a time window.",
        schema: || {
            let mut schema = temporal_schema();
            let object = schema
                .as_object_mut()
                .expect("the temporal schema is an object");
            object["properties"]["id"] = id_schema();
            object["required"] = json!(["id", "timerel", "timeAt"]);
            schema
        },
        result_key: "entity",
    },
    Tool {
        name: "batch_query",
        operations: &[Operation::QueryBatch],
        description: "Query many entities of this context space by id or type in one call.",
        schema: || {
            // The same selectors as `query_entities` — CIM 009 makes the POST form a way to send a
            // long selector, not a narrower one (AG-84). `ids` is kept beside `id` because clients
            // written against the older catalogue send it.
            let mut properties = properties_of(&[SELECTOR_ARGS, REPRESENTATION_ARGS]);
            properties.insert("ids".to_owned(), id_list_schema());
            json!({
                "type": "object",
                "properties": properties,
                "anyOf": [
                    { "required": ["ids"] },
                    { "required": ["id"] },
                    { "required": ["type"] },
                    { "required": ["attrs"] },
                    { "required": ["q"] },
                    { "required": ["georel"] },
                ],
                "additionalProperties": false,
            })
        },
        result_key: "entities",
    },
    Tool {
        name: "list_subscriptions",
        operations: &[Operation::QuerySubscription],
        description: "The context subscriptions of this space the caller may see.",
        schema: || {
            // Paged like every other listing (AG-84): a caller who may see many subscriptions
            // needs the next page and the total as much as a caller who queries entities does.
            json!({
                "type": "object",
                "properties": {
                    "limit": { "type": "integer", "minimum": 1, "maximum": 1000 },
                    "cursor": { "type": "integer", "minimum": 0, "description": "Rows to skip; the previous page's nextCursor" },
                    "count": { "type": "boolean", "description": "Also report the total the grant lets this caller see" },
                },
                "additionalProperties": false,
            })
        },
        result_key: "subscriptions",
    },
    Tool {
        name: "describe_access",
        operations: &[],
        description:
            "The caller's effective grants here: operations, attributes, residual constraints. \
             `format` picks the language: `permissions` (AuthZEN, the default), `odrl` (ODRL 2.2) \
             or `grant-ast` (UCAST); the grants are the same in all three (EP-60).",
        schema: || {
            json!({
                "type": "object",
                "properties": {
                    "format": { "type": "string", "enum": ACCESS_FORMATS },
                },
                "additionalProperties": false,
            })
        },
        result_key: "access",
    },
    Tool {
        name: "describe_schema",
        // The schema is the description of what a read returns, so any read is enough to
        // be shown it; what it then contains is projected to the grant (EP-47).
        operations: &[
            Operation::QueryEntity,
            Operation::RetrieveEntity,
            Operation::RetrieveEntityTypes,
        ],
        description:
            "Inspect the data model of this context space, narrowed to the caller's grant. \
             Read linkml first: it carries the classes, their slots and their descriptions in \
             one small document. The default `summary` lists every artifact with its size and \
             digest and says which to read.",
        schema: || {
            json!({
                "type": "object",
                "properties": {
                    "format": {
                        "type": "string",
                        "enum": [
                            "summary", "json-schema", "context",
                            "linkml", "shacl", "owl", "rdf", "markdown", "qb",
                        ],
                        "description": "summary (the default) lists the models and every artifact with its format, size and digest, linkml first; the others render one, the text formalisms as {format, mediaType, document}; qb only where the model is a data cube",
                    },
                    "entityType": {
                        "oneOf": [
                            type_schema(),
                            {
                                "type": "array",
                                "items": type_schema(),
                                "minItems": 1,
                                "maxItems": 64,
                            },
                        ],
                        "description": "One entity type, or the list of the classes you need",
                    },
                    "version": { "type": "integer", "minimum": 1, "description": "Model major version" },
                },
                "additionalProperties": false,
            })
        },
        result_key: "schema",
    },
    Tool {
        name: "upsert_entity",
        operations: &[Operation::UpsertBatch],
        description: "Create or update one entity of this context space.",
        schema: || {
            json!({
                "type": "object",
                "properties": {
                    "entity": { "type": "object", "description": "The NGSI-LD entity, id and type included" },
                },
                "required": ["entity"],
                "additionalProperties": false,
            })
        },
        result_key: "entity",
    },
    Tool {
        name: "batch_query_temporal",
        operations: &[Operation::QueryTemporal],
        description: "The history of many entities of this context space in one call.",
        schema: || {
            // The POST form of Query Temporal Evolution: the same selectors and the same temporal
            // grammar as `query_temporal`, sent as a body (CIM 009 5.6.12, AG-84).
            let mut properties = properties_of(&[SELECTOR_ARGS, REPRESENTATION_ARGS, TEMPORAL_ARGS]);
            properties.remove("entityMap");
            json!({
                "type": "object",
                "properties": properties,
                "required": ["timerel", "timeAt"],
                "additionalProperties": false,
            })
        },
        result_key: "entities",
    },
    Tool {
        name: "get_type",
        // `GET /types/{type}` is Entity Type Information (CIM 009 5.7.11); the details of the
        // *list* are `GET /types?details=true`, which `list_types` takes. A grant holding the
        // one does not open the other (`operations.rs`).
        operations: &[Operation::RetrieveEntityTypeInfo],
        description: "What one entity type of this context space holds: its attributes and their kinds.",
        schema: || {
            json!({
                "type": "object",
                "properties": { "type": type_schema() },
                "required": ["type"],
                "additionalProperties": false,
            })
        },
        result_key: "entityType",
    },
    Tool {
        name: "get_attribute",
        operations: &[Operation::RetrieveAttrTypeInfo],
        description: "What one attribute of this context space is: its value kinds and the types that carry it.",
        schema: || {
            json!({
                "type": "object",
                "properties": {
                    "attrId": {
                        "type": "string",
                        "maxLength": 256,
                        "description": "The attribute name, as list_attributes spells it",
                    },
                },
                "required": ["attrId"],
                "additionalProperties": false,
            })
        },
        result_key: "attribute",
    },
    Tool {
        name: "get_subscription",
        operations: &[Operation::RetrieveSubscription],
        description: "One context subscription of this space, by its id.",
        schema: || {
            json!({
                "type": "object",
                "properties": {
                    "id": {
                        "type": "string",
                        "maxLength": 512,
                        "description": "The subscription's id, as list_subscriptions gives it",
                    },
                },
                "required": ["id"],
                "additionalProperties": false,
            })
        },
        result_key: "subscription",
    },
    Tool {
        name: "create_subscription",
        operations: &[Operation::CreateSubscription],
        description: "Create a context subscription",
        schema: || {
            json!({
                "type": "object",
                "properties": {
                    "subscription": { "type": "object", "description": "The NGSI-LD Subscription, entities and notification included" },
                },
                "required": ["subscription"],
                "additionalProperties": false,
            })
        },
        result_key: "subscription",
    },
];

/// One argument of the shared read table: its name and the JSON Schema fragment the tool
/// publishes for it (AG-84, AG-31).
struct ReadArg {
    name: &'static str,
    schema: fn() -> Value,
}

/// The entity selectors of the shared table (Architecture/07 §2, "The shared read parameter table").
///
/// Two CIM 009 parameters of this table are not arguments here, because neither surface serves
/// them yet: `accept`, which changes the shape the answer narrowing reads, and `context`, which
/// needs the `@context` allow-list no Endpoint manifest carries today (T-2299). Both are refused
/// as unknown arguments until they arrive on the two surfaces together, with their rules.
const SELECTOR_ARGS: &[ReadArg] = &[
    ReadArg {
        name: "type",
        schema: type_selection_schema,
    },
    ReadArg {
        name: "id",
        schema: id_list_schema,
    },
    ReadArg {
        name: "idPattern",
        schema: || {
            json!({
                "type": "string", "maxLength": 256,
                "description": "Regular expression over entity ids; valid only beside type, attrs, q or georel",
            })
        },
    },
    ReadArg {
        name: "attrs",
        schema: attrs_schema,
    },
    ReadArg {
        name: "pick",
        schema: || member_list_schema("Members to keep, id, type and scope included"),
    },
    ReadArg {
        name: "omit",
        schema: || member_list_schema("Members to leave out"),
    },
    ReadArg {
        name: "q",
        schema: || {
            json!({
                "type": "string", "maxLength": 4096,
                "description": "NGSI-LD query filter, e.g. pm25>35",
            })
        },
    },
    ReadArg {
        name: "csf",
        schema: || {
            json!({
                "type": "string", "maxLength": 1024,
                "description": "Context source filter; only where this endpoint federates",
            })
        },
    },
    ReadArg {
        name: "georel",
        schema: || {
            json!({
                "type": "string", "maxLength": 256,
                "description": "NGSI-LD geo relation, e.g. near;maxDistance==2000",
            })
        },
    },
    ReadArg {
        name: "geometry",
        schema: || {
            json!({
                "type": "string",
                "enum": ["Point", "LineString", "Polygon", "MultiPoint", "MultiLineString", "MultiPolygon"],
            })
        },
    },
    ReadArg {
        name: "coordinates",
        schema: || {
            json!({
                "type": "string", "maxLength": 8192,
                "description": "GeoJSON coordinates of the reference geometry",
            })
        },
    },
    ReadArg {
        // Judged against the grants, never forwarded on its own: an area a grant draws is
        // drawn on `location`, so a geo query moved to another GeoProperty is refused by name
        // rather than answered inside that area (T-2299, AG-85).
        name: "geoproperty",
        schema: || {
            json!({
                "type": "string", "maxLength": 256, "pattern": ATTR_NAME,
                "description": "The GeoProperty the geo query applies to; location when absent, and only where the grant draws no area of its own",
            })
        },
    },
    ReadArg {
        name: "scopeQ",
        schema: || {
            json!({
                "type": "string", "maxLength": 1024,
                "description": "NGSI-LD scope query, e.g. /geo/SK/BB",
            })
        },
    },
    ReadArg {
        name: "lang",
        schema: || {
            json!({
                "type": "string", "maxLength": 64,
                "description": "Language filter for a LanguageProperty",
            })
        },
    },
    ReadArg {
        name: "datasetId",
        schema: || {
            json!({
                "type": "array", "maxItems": 50,
                "items": { "type": "string", "maxLength": 512 },
                "description": "Dataset instances; @none selects the default instance",
            })
        },
    },
    ReadArg {
        name: "join",
        schema: || {
            json!({
                "type": "string", "enum": ["inline", "flat", "@none"],
                "description": "Retrieve linked entities",
            })
        },
    },
    ReadArg {
        name: "joinLevel",
        schema: || {
            json!({
                "type": "integer", "minimum": 1, "maximum": 3,
                "description": "How many relationship hops a join follows",
            })
        },
    },
    ReadArg {
        name: "containedBy",
        schema: || {
            json!({
                "type": "array", "maxItems": 50,
                "items": { "type": "string", "maxLength": 512, "pattern": ENTITY_URN },
                "description": "Loop protection for a hierarchical join",
            })
        },
    },
    ReadArg {
        name: "entityMap",
        schema: || {
            json!({
                "type": "boolean",
                "description": "Keep a federated entity map for paging; only where this endpoint federates",
            })
        },
    },
    ReadArg {
        name: "local",
        schema: || {
            json!({
                "type": "boolean", "description": "Answer from this broker only, without its federated sources",
            })
        },
    },
    ReadArg {
        name: "count",
        schema: || {
            json!({
                "type": "boolean", "description": "Also report the total the grant lets this caller see",
            })
        },
    },
    ReadArg {
        name: "limit",
        schema: || {
            json!({
                "type": "integer", "minimum": 1, "maximum": 1000,
            })
        },
    },
    ReadArg {
        name: "cursor",
        schema: || {
            json!({
                "type": "integer", "minimum": 0,
                "description": "Rows to skip; the previous page's nextCursor",
            })
        },
    },
];

/// The projection arguments a read of one named entity takes: it selects nothing, so `type`, `q` and
/// the geo query have no meaning, but every way of narrowing and writing the answer does (AG-84).
const PROJECTION_ARGS: &[ReadArg] = &[
    ReadArg {
        name: "attrs",
        schema: attrs_schema,
    },
    ReadArg {
        name: "pick",
        schema: || member_list_schema("Members to keep, id, type and scope included"),
    },
    ReadArg {
        name: "omit",
        schema: || member_list_schema("Members to leave out"),
    },
    ReadArg {
        name: "lang",
        schema: || {
            json!({
                "type": "string", "maxLength": 64,
                "description": "Language filter for a LanguageProperty",
            })
        },
    },
    ReadArg {
        name: "datasetId",
        schema: || {
            json!({
                "type": "array", "maxItems": 50,
                "items": { "type": "string", "maxLength": 512 },
                "description": "Dataset instances; @none selects the default instance",
            })
        },
    },
    ReadArg {
        name: "join",
        schema: || {
            json!({
                "type": "string", "enum": ["inline", "flat", "@none"],
                "description": "Retrieve linked entities",
            })
        },
    },
    ReadArg {
        name: "joinLevel",
        schema: || {
            json!({
                "type": "integer", "minimum": 1, "maximum": 3,
                "description": "How many relationship hops a join follows",
            })
        },
    },
    ReadArg {
        name: "containedBy",
        schema: || {
            json!({
                "type": "array", "maxItems": 50,
                "items": { "type": "string", "maxLength": 512, "pattern": ENTITY_URN },
                "description": "Loop protection for a hierarchical join",
            })
        },
    },
    ReadArg {
        name: "local",
        schema: || {
            json!({
                "type": "boolean", "description": "Answer from this broker only, without its federated sources",
            })
        },
    },
];

/// How the answer is written, on every tool that returns entities (Architecture/07 §2).
const REPRESENTATION_ARGS: &[ReadArg] = &[
    ReadArg {
        name: "format",
        schema: || {
            json!({
                "type": "string",
                "enum": ["normalized", "simplified", "keyValues", "concise", "temporalValues", "aggregatedValues"],
                "description": "How each entity is written; normalized when absent",
            })
        },
    },
    ReadArg {
        name: "options",
        schema: || {
            json!({
                "type": "array", "maxItems": 8,
                "items": { "type": "string", "enum": ["keyValues", "concise", "sysAttrs", "normalized", "temporalValues", "aggregatedValues"] },
                "description": "NGSI-LD options; sysAttrs adds the system members REST adds",
            })
        },
    },
    ReadArg {
        // The value the broker copies into a GeoJSON `geometry` is no longer that attribute,
        // so the answer narrowing cannot cut it: the attribute has to be one the grant covers
        // before the request is sent (T-2299, AG-85).
        name: "geometryProperty",
        schema: || {
            json!({
                "type": "string", "maxLength": 256, "pattern": ATTR_NAME,
                "description": "The GeoProperty that becomes the geometry of a GeoJSON answer; only an attribute this endpoint serves you",
            })
        },
    },
];

/// The `type` argument of the shared table: one name, a list of names, or a selection expression.
fn type_selection_schema() -> Value {
    json!({
        "description": "NGSI-LD entity type: one name, several, or a selection expression (A,B for or; A;B for and)",
        "oneOf": [
            // A selection expression is type names and the operators that join them, and nothing
            // else: the same reason `TYPE_NAME` is a pattern rather than a length (AG-21). A value
            // carrying a slash, a dot, a percent or a space cannot denote a type, so it is refused
            // by name instead of being answered `[]` — which a probe reads as "it went through".
            { "type": "string", "maxLength": 1024, "pattern": TYPE_SELECTION },
            { "type": "array", "maxItems": 50, "items": type_schema() },
        ],
    })
}

/// The `id` argument where CIM 009 takes a list of entity ids.
fn id_list_schema() -> Value {
    json!({
        "type": "array",
        "maxItems": 100,
        "items": { "type": "string", "maxLength": 512, "pattern": ENTITY_URN },
        "description": "Entity URNs to select",
    })
}

fn member_list_schema(description: &str) -> Value {
    json!({
        "type": "array",
        "maxItems": 256,
        "items": { "type": "string", "maxLength": 256 },
        "description": description,
    })
}

/// The properties of a tool that takes these arguments, as its published JSON Schema holds them.
fn properties_of(groups: &[&[ReadArg]]) -> Map<String, Value> {
    let mut properties = Map::new();
    for group in groups {
        for arg in *group {
            properties.insert(arg.name.to_owned(), (arg.schema)());
        }
    }
    properties
}

/// The `type` argument, which every tool that takes one spells the same way (AG-21).
fn type_schema() -> Value {
    json!({
        "type": "string",
        "maxLength": 256,
        "pattern": TYPE_NAME,
        "description": "NGSI-LD entity type, e.g. AirQualityObserved",
    })
}

/// The `id` argument: one entity, named by the URN of ADR 001 (AG-21).
fn id_schema() -> Value {
    json!({
        "type": "string",
        "maxLength": 512,
        "pattern": ENTITY_URN,
        "description": "Entity URN, urn:ngsi-ld:{Type}:{domain}:{space}:{localId}",
    })
}

/// The `attrs` argument, which every read tool spells the same way.
fn attrs_schema() -> Value {
    json!({
        "type": "array",
        // Bounded like every other argument (T-0972): the broker parses what arrives, so an
        // unbounded list is a way to spend its memory through a tool the grant allows.
        "maxItems": 256,
        "items": { "type": "string", "maxLength": 256 },
        "description": "The attributes to return; all the grant covers when absent",
    })
}

/// The temporal query grammar, forwarded to the broker unchanged (AG-30).
///
/// One schema for both temporal tools: `retrieve_temporal` is this plus a required `id`,
/// which is the only difference CIM 009 makes between them.
fn temporal_schema() -> Value {
    let mut properties = properties_of(&[SELECTOR_ARGS, REPRESENTATION_ARGS, TEMPORAL_ARGS]);
    properties.remove("entityMap");
    json!({
        "type": "object",
        "properties": properties,
        "required": ["timerel", "timeAt"],
        "additionalProperties": false,
    })
}

/// The temporal grammar of the shared table, forwarded to the broker as written (AG-30, AG-84).
const TEMPORAL_ARGS: &[ReadArg] = &[
    ReadArg {
        name: "timerel",
        schema: || {
            json!({
                "type": "string", "enum": ["before", "after", "between"],
            })
        },
    },
    ReadArg {
        name: "timeAt",
        schema: || {
            json!({
                "type": "string", "maxLength": 64, "description": "ISO 8601 instant",
            })
        },
    },
    ReadArg {
        name: "endTimeAt",
        schema: || {
            json!({
                "type": "string", "maxLength": 64, "description": "ISO 8601 instant, with timerel=between",
            })
        },
    },
    ReadArg {
        name: "timeproperty",
        schema: || {
            json!({
                "type": "string", "enum": ["observedAt", "createdAt", "modifiedAt", "deletedAt"],
                "description": "The temporal property; observedAt when absent",
            })
        },
    },
    ReadArg {
        name: "lastN",
        schema: || {
            json!({
                "type": "integer", "minimum": 1,
                "description": "Keep only the N most recent instances per attribute",
            })
        },
    },
    ReadArg {
        name: "aggrMethods",
        schema: || {
            json!({
                "type": "array", "maxItems": 8,
                "items": { "type": "string", "enum": ["totalCount", "distinctCount", "sum", "avg", "min", "max", "stddev", "sumsq"] },
                "description": "Aggregate instead of returning every instance",
            })
        },
    },
    ReadArg {
        name: "aggrPeriodDuration",
        schema: || {
            json!({
                "type": "string", "maxLength": 64,
                "description": "ISO 8601 duration of one aggregation bucket, e.g. PT1H",
            })
        },
    },
];

/// One compiled validator per tool, in the order of `TOOLS` (AG-31).
///
/// Compiled once rather than per call: the schemas are constants, and an agent that asks
/// twice pays for the compilation once. A schema that does not compile leaves its tool
/// without a validator, and `validate` then refuses every call to it rather than letting
/// unvalidated arguments through.
static VALIDATORS: LazyLock<Vec<Option<jsonschema::Validator>>> = LazyLock::new(|| {
    TOOLS
        .iter()
        .map(|tool| jsonschema::draft7::new(&(tool.schema)()).ok())
        .collect()
});

/// Checks one tool's arguments against its published JSON Schema before anything is built
/// from them (AG-21, AG-31).
///
/// `additionalProperties: false` is in every schema, so an unknown field is an error here
/// rather than a parameter that is silently dropped and an answer that quietly means
/// something else.
pub(crate) fn validate(index: usize, arguments: &Map<String, Value>) -> Result<(), String> {
    let Some(Some(validator)) = VALIDATORS.get(index) else {
        return Err(
            "this tool's schema does not compile, so its arguments cannot be checked".to_owned(),
        );
    };
    let instance = Value::Object(arguments.clone());
    let problems: Vec<String> = validator
        .iter_errors(&instance)
        .map(|error| {
            let at = error.instance_path().to_string();
            let rule = broken_rule(&error);
            match at.is_empty() {
                true => rule,
                false => format!("{at}: {rule}"),
            }
        })
        .collect();
    match problems.is_empty() {
        true => Ok(()),
        false => Err(problems.join("; ")),
    }
}

/// Names the rule an argument broke, without repeating what the caller sent (GW31, AG-21).
///
/// A validator writes the offending value into its own message, and a tool error is read by a
/// model: a refusal that quotes the argument carries whatever was typed into the next prompt.
/// The place and the rule are what a caller needs to correct the call; the value it already has.
fn broken_rule(error: &jsonschema::ValidationError<'_>) -> String {
    use jsonschema::error::ValidationErrorKind as Rule;
    match error.kind() {
        Rule::AdditionalProperties { unexpected } => {
            format!(
                "no argument of this tool is called {}",
                unexpected.join(", ")
            )
        }
        Rule::Required { property } => format!("{property} is required"),
        // A list of alternatives, each one argument the call has to name (CIM 009 5.7.2.4): the
        // names are what the caller needs, not the keyword (T-3044).
        Rule::AnyOf { context } => {
            let names: Vec<&str> = context
                .iter()
                .filter_map(|branch| match branch.as_slice() {
                    [only] => match only.kind() {
                        Rule::Required { property } => property.as_str(),
                        _ => None,
                    },
                    _ => None,
                })
                .collect();
            match !names.is_empty() && names.len() == context.len() {
                true => format!(
                    "names none of {}: one of them is required",
                    names.join(", ")
                ),
                false => "matches none of the shapes the schema allows".to_owned(),
            }
        }
        Rule::Type { .. } => "is not of the type the schema declares".to_owned(),
        Rule::Pattern { pattern } => format!("does not match {pattern}"),
        Rule::MaxLength { limit } => format!("is longer than {limit} characters"),
        Rule::MinLength { limit } => format!("is shorter than {limit} characters"),
        Rule::MaxItems { limit } => format!("has more than {limit} items"),
        Rule::MinItems { limit } => format!("has fewer than {limit} items"),
        Rule::Maximum { limit } => format!("is above {limit}"),
        Rule::Minimum { limit } => format!("is below {limit}"),
        Rule::Enum { options } => format!("is not one of {options}"),
        Rule::Format { format } => format!("is not a {format}"),
        _ => {
            let keyword = error.schema_path().to_string();
            let keyword = keyword.rsplit('/').next().unwrap_or_default().to_owned();
            match keyword.is_empty() {
                true => "is not what the schema allows".to_owned(),
                false => format!("breaks the schema's {keyword}"),
            }
        }
    }
}

/// Whether the caller holds any of the tool's operations, asked of the PDP that also
/// enforces them, so discovery and enforcement cannot drift (SP-16).
pub(crate) fn granted(
    gateway: &Gateway,
    endpoint: &Endpoint,
    subject: &Subject,
    operations: &[Operation],
) -> bool {
    operations.is_empty()
        || operations.iter().any(|operation| {
            !matches!(
                gateway
                    .pdp
                    .decide(subject, *operation, &PolicyRequest::default(), endpoint),
                Verdict::Deny
            )
        })
}

/// Whether the caller may read anything on this Endpoint: a data tool its grants cover. An
/// Endpoint that answers only the describing tools is not one the hub lists (ADR-N-025 2.1).
pub(crate) fn reads_anything(gateway: &Gateway, endpoint: &Endpoint, subject: &Subject) -> bool {
    TOOLS.iter().any(|tool| {
        !tool.operations.is_empty() && granted(gateway, endpoint, subject, tool.operations)
    })
}

/// The hub's catalogue (ADR-N-025 2.2, 2.8): each tool once, listed when at least one Endpoint of
/// the caller's list grants it, with the one required `endpoint` argument whose enum is that list.
/// `tools/list` never multiplies by Endpoint.
pub(crate) fn hub_tools(gateway: &Gateway, reach: &[(Arc<Endpoint>, Subject)]) -> Vec<Value> {
    let slugs: Vec<&str> = reach
        .iter()
        .map(|(endpoint, _)| endpoint.slug.as_str())
        .collect();
    TOOLS
        .iter()
        .filter(|tool| {
            reach
                .iter()
                .any(|(endpoint, subject)| granted(gateway, endpoint, subject, tool.operations))
        })
        .map(|tool| {
            let mut schema = (tool.schema)();
            if let Value::Object(object) = &mut schema {
                if let Some(Value::Object(properties)) = object.get_mut("properties") {
                    properties.insert(
                        "endpoint".to_owned(),
                        json!({
                            "type": "string",
                            "enum": slugs,
                            "description": "The Endpoint this call reads or writes, by the slug \
                                            list_endpoints answers. One per call.",
                        }),
                    );
                }
                match object.get_mut("required") {
                    Some(Value::Array(required)) => required.push(json!("endpoint")),
                    _ => {
                        object.insert("required".to_owned(), json!(["endpoint"]));
                    }
                }
            }
            json!({
                "name": tool.name,
                "description": format!(
                    "{} Name the Endpoint in `endpoint`; the call is that Endpoint's own.",
                    tool.description
                ),
                "inputSchema": schema,
                "outputSchema": output_schema(tool),
                "annotations": {
                    "readOnlyHint": tool.read_only(),
                    "destructiveHint": !tool.read_only(),
                },
            })
        })
        .collect()
}

/// How the hub stands with one tool on the Endpoint a call names (ADR-N-025 2.8, SP-17, SP-20).
pub(crate) enum HubTool {
    /// No such tool, or none of the caller's Endpoints grants it: the hub never listed it.
    Unknown,
    /// Listed, because another Endpoint of the caller's list grants it, and not granted here:
    /// the tool error that names the Endpoint and the operation.
    Ungranted(Value),
    /// The Endpoint grants it; the call goes on as that Endpoint's own.
    Granted,
}

/// Where `name` stands on `endpoint` for a caller whose hub list is `reach`.
pub(crate) fn hub_tool(
    gateway: &Gateway,
    endpoint: &Endpoint,
    subject: &Subject,
    reach: &[(Arc<Endpoint>, Subject)],
    name: &str,
) -> HubTool {
    let Some(tool) = TOOLS.iter().find(|tool| tool.name == name) else {
        return HubTool::Unknown;
    };
    if granted(gateway, endpoint, subject, tool.operations) {
        return HubTool::Granted;
    }
    let listed = reach
        .iter()
        .any(|(other, holder)| granted(gateway, other, holder, tool.operations));
    if !listed {
        return HubTool::Unknown;
    }
    let operations: Vec<&str> = tool.operations.iter().map(Operation::as_str).collect();
    HubTool::Ungranted(refused(
        &format!(
            "the Endpoint `{}` does not grant {} to this caller; `{name}` answers only on the \
             Endpoints whose Policy grants it (SP-17)",
            endpoint.slug,
            operations.join(" or ")
        ),
        &Value::Null,
    ))
}

/// What a tool's `structuredContent` looks like, published beside its input schema so a
/// client can validate the half it parses (MCP `outputSchema`).
///
/// The payload's own shape is the data model's, not MCP's — a schema here would be a second
/// copy of it that drifts — so the output schema names what the object carries and leaves the
/// value to `describe_schema`.
pub(crate) fn output_schema(tool: &Tool) -> Value {
    json!({
        "type": "object",
        "properties": {
            tool.result_key: { "description": tool.description },
            "restricted": {
                "type": "boolean",
                "description": "Present when the policy narrowed this answer (AG-13, R22)",
            },
            "jc:source": {
                "type": "array",
                "items": { "type": "string" },
                "description": "The registrations this answer is a union over, by name (EP-71)",
            },
        },
        "required": [tool.result_key],
    })
}

#[cfg(test)]
mod tests {
    use super::{Tool, TOOLS};
    use jc_core::kinds::Operation;

    /// T-1066, AG-07: `read_only` is the `readOnlyHint` an agent runtime reads before it asks a
    /// person, so it is derived from the operations rather than set beside them. The two writing
    /// tools are named here, because a tool added on the wrong side of this line is a write that
    /// is announced as a read.
    #[test]
    fn a_tool_that_writes_at_all_is_not_read_only() {
        let writing: Vec<&str> = TOOLS
            .iter()
            .filter(|tool| !tool.read_only())
            .map(|tool| tool.name)
            .collect();
        assert_eq!(writing, vec!["upsert_entity", "create_subscription"]);

        for tool in TOOLS {
            let writes = tool.operations.iter().any(Operation::is_write);
            assert_eq!(
                tool.read_only(),
                !writes,
                "{} announces the wrong side of readOnlyHint",
                tool.name
            );
        }
    }

    /// T-3044: a query that names no selector is told which ones it may name. "breaks the
    /// schema's anyOf" cost the assistant a call on dev to find out.
    #[test]
    fn a_query_without_a_selector_is_told_the_selectors_it_may_name() {
        let index = TOOLS
            .iter()
            .position(|tool| tool.name == "query_entities")
            .expect("query_entities");
        let arguments = serde_json::json!({ "count": true, "limit": 2 });
        let problem = super::validate(index, arguments.as_object().expect("an object"))
            .expect_err("no selector");
        assert_eq!(
            problem,
            "names none of type, id, attrs, q, georel: one of them is required"
        );
    }

    /// A tool that stands for no operation describes the endpoint rather than its data, and
    /// describing is reading (EP-55).
    #[test]
    fn a_tool_with_no_operation_of_its_own_reads() {
        for tool in TOOLS.iter().filter(|tool| tool.operations.is_empty()) {
            assert!(tool.read_only(), "{}", tool.name);
        }
    }

    /// The rule the task proposed, read-only when *any* operation reads, would announce a mixed
    /// tool as a read. This is the case that tells the two rules apart.
    #[test]
    fn a_tool_that_both_reads_and_writes_is_not_read_only() {
        let mixed = Tool {
            name: "read_and_write",
            operations: &[Operation::QueryEntity, Operation::UpsertBatch],
            description: "",
            schema: || serde_json::json!({}),
            result_key: "entities",
        };
        assert!(!mixed.read_only());
    }
}
