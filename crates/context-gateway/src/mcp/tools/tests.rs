use super::{
    anonymous_bounds, anonymous_may, anonymous_refusal, duration_seconds, keep_anonymous, Shape,
    Tool, ANONYMOUS_TOOLS, TOOLS,
};
use jc_core::kinds::Operation;
use serde_json::{json, Map, Value};

/// A day of AirQualityObserved history, the one shape an anonymous caller is let through.
fn day() -> Map<String, Value> {
    json!({
        "type": ["AirQualityObserved"],
        "timerel": "between",
        "timeAt": "2026-10-12T00:00:00Z",
        "endTimeAt": "2026-10-13T00:00:00Z",
    })
    .as_object()
    .cloned()
    .expect("an object")
}

fn bounded(name: &str, change: Value) -> Result<Map<String, Value>, String> {
    let mut arguments = day();
    for (key, value) in change.as_object().expect("an object") {
        if value.is_null() {
            arguments.remove(key);
        } else {
            arguments.insert(key.clone(), value.clone());
        }
    }
    anonymous_bounds(name, &mut arguments).map(|()| arguments)
}

/// T-4261: a window of 31 days passes, one second more is refused, by its bound.
#[test]
fn an_anonymous_window_over_31_days_is_refused() {
    assert!(bounded(
        "query_temporal",
        json!({ "endTimeAt": "2026-11-12T00:00:00Z" })
    )
    .is_ok());
    let refused = bounded(
        "query_temporal",
        json!({ "endTimeAt": "2026-11-12T00:00:01Z" }),
    )
    .expect_err("32 days are refused");
    assert!(refused.contains("at most 31 days"), "{refused}");
}

/// T-4261: `before`, `after`, a missing end, an end before the start and an unparsable
/// instant are no window.
#[test]
fn an_anonymous_read_without_a_window_is_refused() {
    for change in [
        json!({ "timerel": "before", "endTimeAt": null }),
        json!({ "timerel": "after", "endTimeAt": null }),
        json!({ "endTimeAt": null }),
        json!({ "endTimeAt": "2026-10-11T00:00:00Z" }),
        json!({ "timeAt": "yesterday" }),
        json!({ "timerel": null }),
    ] {
        let refused = bounded("retrieve_temporal", change.clone()).expect_err("no window");
        assert!(refused.contains("timerel between"), "{change}: {refused}");
    }
}

/// T-4261, decided: `lastN` above 100 is refused, not clamped, so the caller learns why its
/// answer is short; an absent one becomes 100.
#[test]
fn an_anonymous_last_n_is_100_when_absent_and_more_is_refused() {
    assert_eq!(
        bounded("query_temporal", json!({})).expect("ok")["lastN"],
        json!(100)
    );
    assert_eq!(
        bounded("query_temporal", json!({ "lastN": 7 })).expect("ok")["lastN"],
        json!(7)
    );
    // T-4800: a window over a day asking no raw values keeps lastN absent, so the broker
    // answers hourly averages; asking raw (format) or naming aggrMethods keeps the cap
    let week = json!({ "endTimeAt": "2026-10-19T00:00:00Z" });
    assert!(!bounded("query_temporal", week)
        .expect("ok")
        .contains_key("lastN"));
    let raw = json!({ "endTimeAt": "2026-10-19T00:00:00Z", "format": "temporalValues" });
    assert_eq!(
        bounded("query_temporal", raw).expect("ok")["lastN"],
        json!(100)
    );
    let own = json!({ "endTimeAt": "2026-10-19T00:00:00Z", "aggrMethods": "avg", "aggrPeriodDuration": "PT1H" });
    assert_eq!(
        bounded("retrieve_temporal", own).expect("ok")["lastN"],
        json!(100)
    );
    for n in [json!(1000), json!(101), json!(0), json!("5")] {
        let refused = bounded("query_temporal", json!({ "lastN": n })).expect_err("refused");
        assert!(
            refused.contains("`lastN` without a token is 1 to 100"),
            "{refused}"
        );
    }
}

/// T-4261: a history read over every Entity names neither `type` nor `id`.
#[test]
fn an_anonymous_read_without_type_or_id_is_refused() {
    for change in [
        json!({ "type": null }),
        json!({ "type": [] }),
        json!({ "type": null, "id": [] }),
    ] {
        let refused = bounded("query_temporal", change).expect_err("refused");
        assert!(refused.contains("names `type` or `id`"), "{refused}");
    }
    assert!(bounded(
        "query_temporal",
        json!({ "type": null, "id": ["urn:ngsi-ld:AirQualityObserved:1"] })
    )
    .is_ok());
    assert!(bounded("query_temporal", json!({ "limit": 1000 })).is_ok());
    assert!(bounded("query_temporal", json!({ "limit": 1001 })).is_err());
}

/// T-4261: aggregation periods under an hour, and aggregation with no period, are refused.
#[test]
fn an_anonymous_aggregation_period_under_an_hour_is_refused() {
    for period in [
        json!("PT1M"),
        json!("PT59M59S"),
        json!("P"),
        json!("1H"),
        Value::Null,
    ] {
        let refused = bounded(
            "query_temporal",
            json!({ "aggrMethods": "avg", "aggrPeriodDuration": period }),
        )
        .expect_err("refused");
        assert!(refused.contains("at least PT1H"), "{refused}");
    }
    for period in ["PT1H", "PT60M", "P1D", "P1W", "P1M", "PT0.5H30M"] {
        assert!(
            bounded(
                "query_temporal",
                json!({ "aggrMethods": "avg", "aggrPeriodDuration": period })
            )
            .is_ok(),
            "{period}"
        );
    }
}

#[test]
fn durations_parse_as_iso_8601_and_nothing_else() {
    assert_eq!(duration_seconds("PT1H"), Some(3600.0));
    assert_eq!(duration_seconds("P1DT1M"), Some(86_460.0));
    assert_eq!(duration_seconds("PT1.5S"), Some(1.5));
    for bad in ["", "P", "PT", "P1H", "PT1D", "PT1M1H", "P1", "PTH", "P-1D"] {
        assert_eq!(duration_seconds(bad), None, "{bad}");
    }
}

/// T-4261: the bounds are the temporal tools' alone.
#[test]
fn the_bounds_leave_every_other_tool_untouched() {
    let mut arguments = Map::new();
    assert!(anonymous_bounds("query_entities", &mut arguments).is_ok());
    assert!(arguments.is_empty());
}

/// T-4168: every tool an anonymous caller may use reads, and every tool that writes, or that
/// nobody named, is kept from it.
#[test]
fn an_anonymous_caller_may_use_the_named_reads_alone() {
    for name in ANONYMOUS_TOOLS.iter().filter(|name| {
        !matches!(
            **name,
            "list_endpoints" | "get_endpoint" | "search" | "search_documents"
        )
    }) {
        let tool = TOOLS.iter().find(|tool| tool.name == *name);
        assert!(
            tool.is_some_and(Tool::read_only),
            "{name} is not a read of the catalogue"
        );
    }
    for tool in TOOLS.iter().filter(|tool| !tool.read_only()) {
        assert!(!anonymous_may(tool.name), "{} writes", tool.name);
    }
    // The gateway's own document search reads only (T-4659).
    assert_eq!(
        crate::mcp::documents::tool("")["annotations"]["readOnlyHint"],
        true
    );
    for name in [
        "get_type",
        "list_type_details",
        "list_attributes",
        "list_attribute_details",
        "get_attribute",
        "query_temporal",
    ] {
        assert!(
            anonymous_may(name),
            "{name} is a read an anonymous caller is refused"
        );
    }
    for name in [
        "create_subscription",
        "delete_entity",
        "a_tool_added_later",
        "",
    ] {
        assert!(!anonymous_may(name), "{name} is admitted");
    }
}

#[test]
fn an_anonymous_listing_keeps_the_reads_and_drops_writes_and_nameless_entries() {
    let mut tools = vec![
        json!({"name": "query_entities"}),
        json!({"name": "create_subscription"}),
        json!({"name": "update_subscription"}),
        json!({"name": "a_tool_added_later"}),
        json!({"title": "no name"}),
        json!({"name": 7}),
        json!({"name": "describe_access"}),
    ];
    keep_anonymous(&mut tools);
    assert_eq!(
        tools,
        [
            json!({"name": "query_entities"}),
            json!({"name": "describe_access"})
        ]
    );
}

/// T-4680: the broker's `summarize` reads what `query_entities` reads, under the same view and
/// Policy, and answers a profile; a visitor's assistant may list and call it.
#[test]
fn an_anonymous_caller_may_summarize_a_type() {
    assert!(anonymous_may("summarize"));
    let mut tools = vec![
        json!({"name": "summarize"}),
        json!({"name": "delete_entity"}),
    ];
    keep_anonymous(&mut tools);
    assert_eq!(tools, [json!({"name": "summarize"})]);
}

#[test]
fn an_anonymous_refusal_is_a_json_rpc_error_naming_what_it_may_call() {
    let refusal = anonymous_refusal(json!(4), "delete_subscription");
    assert_eq!(refusal["id"], 4);
    assert_eq!(refusal["error"]["code"], -32602);
    let message = refusal["error"]["message"].as_str().unwrap_or_default();
    assert!(message.contains("`delete_subscription`"), "{message}");
    assert!(message.contains("query_entities"), "{message}");
    assert!(refusal.get("result").is_none());
}

/// T-1066, AG-07: `read_only` is the `readOnlyHint` an agent runtime reads before it asks a
/// person, so it is derived from the operations rather than set beside them; and it says what
/// the broker's view says of the same tool (EP-112).
#[test]
fn a_tool_that_writes_at_all_is_not_read_only() {
    for tool in TOOLS {
        let writes = tool.operations.iter().any(Operation::is_write);
        assert_eq!(
            tool.read_only(),
            !writes,
            "{} announces the wrong side of readOnlyHint",
            tool.name
        );
        assert_eq!(
            tool.read_only(),
            READS.contains(&tool.name) || tool.name.starts_with("describe_"),
            "{} reads on one surface and writes on the other",
            tool.name
        );
    }
    let adding: Vec<&str> = TOOLS
        .iter()
        .filter(|tool| !tool.read_only() && !tool.destructive())
        .map(|tool| tool.name)
        .collect();
    assert_eq!(
        adding,
        ["batch_create", "create_entity", "create_subscription"]
    );
}

/// The broker's read tools (Antares `docs/src/mcp.md` section 2).
const READS: &[&str] = &[
    "batch_query",
    "batch_query_temporal",
    "get_attribute",
    "get_attribute_value",
    "get_entity",
    "get_subscription",
    "get_type",
    "list_attribute_details",
    "list_attributes",
    "list_subscriptions",
    "list_type_details",
    "list_types",
    "query_entities",
    "query_temporal",
    "retrieve_temporal",
    "summarize",
];

/// EP-112: the hub lists the broker's tools by the broker's names and argument names, so the
/// call it hands on is the caller's as sent. The pin is Antares `crates/antares-mcp/src/
/// tools.rs` at dev 2026-10-12; a broker tool renamed or an argument added there fails here
/// first, and the conformance diff of the four surfaces catches what this copy misses.
#[test]
fn every_tool_is_the_broker_s_with_the_broker_s_arguments() {
    let pinned: &[(&str, &[&str])] = &[
        ("append_attributes", &["id", "attributes", "noOverwrite"]),
        ("batch_create", &["entities"]),
        ("batch_delete", &["ids"]),
        ("batch_merge", &["entities"]),
        (
            "batch_query",
            &[
                "type",
                "id",
                "attrs",
                "pick",
                "omit",
                "q",
                "scopeQ",
                "georel",
                "geometry",
                "coordinates",
                "geoproperty",
                "local",
                "lang",
                "count",
                "limit",
                "cursor",
            ],
        ),
        (
            "batch_query_temporal",
            &[
                "type",
                "id",
                "attrs",
                "q",
                "scopeQ",
                "georel",
                "geometry",
                "coordinates",
                "geoproperty",
                "timerel",
                "timeAt",
                "endTimeAt",
                "timeproperty",
                "lastN",
                "aggrMethods",
                "aggrPeriodDuration",
                "count",
                "limit",
                "cursor",
            ],
        ),
        ("batch_update", &["entities", "noOverwrite"]),
        ("batch_upsert", &["entities", "options"]),
        ("create_entity", &["entity"]),
        ("create_subscription", &["subscription"]),
        (
            "delete_attribute",
            &["id", "attrId", "datasetId", "deleteAll"],
        ),
        ("delete_entity", &["id"]),
        ("delete_subscription", &["id"]),
        ("get_attribute", &["attrId"]),
        ("get_attribute_value", &["id", "attrId"]),
        (
            "get_entity",
            &[
                "id",
                "attrs",
                "pick",
                "omit",
                "lang",
                "join",
                "joinLevel",
                "format",
                "sysAttrs",
            ],
        ),
        ("get_subscription", &["id"]),
        ("get_type", &["type"]),
        ("list_attribute_details", &[]),
        ("list_attributes", &[]),
        ("list_subscriptions", &["count", "limit", "cursor"]),
        ("list_type_details", &[]),
        ("list_types", &[]),
        ("merge_entity", &["id", "fragment"]),
        (
            "query_entities",
            &[
                "type",
                "id",
                "idPattern",
                "attrs",
                "pick",
                "omit",
                "q",
                "scopeQ",
                "georel",
                "geometry",
                "coordinates",
                "geoproperty",
                "local",
                "lang",
                "join",
                "joinLevel",
                "format",
                "sysAttrs",
                "orderBy",
                "count",
                "limit",
                "cursor",
            ],
        ),
        (
            "query_temporal",
            &[
                "type",
                "id",
                "attrs",
                "q",
                "scopeQ",
                "georel",
                "geometry",
                "coordinates",
                "geoproperty",
                "timerel",
                "timeAt",
                "endTimeAt",
                "timeproperty",
                "lastN",
                "aggrMethods",
                "aggrPeriodDuration",
                "format",
                "sysAttrs",
                "count",
                "limit",
                "cursor",
            ],
        ),
        ("replace_attribute", &["id", "attrId", "attribute"]),
        ("replace_entity", &["id", "entity"]),
        (
            "retrieve_temporal",
            &[
                "id",
                "attrs",
                "timerel",
                "timeAt",
                "endTimeAt",
                "timeproperty",
                "lastN",
                "aggrMethods",
                "aggrPeriodDuration",
                "format",
                "sysAttrs",
            ],
        ),
        (
            "summarize",
            &[
                "type",
                "q",
                "scopeQ",
                "georel",
                "geometry",
                "coordinates",
                "geoproperty",
                "lang",
            ],
        ),
        ("update_attribute", &["id", "attrId", "attribute"]),
        ("update_attributes", &["id", "attributes"]),
        ("update_subscription", &["id", "fragment"]),
    ];
    let ours: Vec<(&str, Vec<&str>)> = TOOLS
        .iter()
        .filter_map(|tool| match tool.shape {
            Shape::Broker(args) => Some((tool.name, args.iter().map(|arg| arg.name).collect())),
            Shape::Own(_) => None,
        })
        .collect();
    let pinned: Vec<(&str, Vec<&str>)> = pinned
        .iter()
        .map(|(name, args)| (*name, args.to_vec()))
        .collect();
    assert_eq!(ours, pinned);
    let own: Vec<&str> = TOOLS
        .iter()
        .filter(|tool| matches!(tool.shape, Shape::Own(_)))
        .map(|tool| tool.name)
        .collect();
    assert_eq!(own, crate::mcp::describe::DESCRIBE_TOOLS);
}

/// T-4258, EP-116: the reads take the standard's representation and ordering, with the
/// temporal formats on the history tools, and the POST history form takes none.
#[test]
fn the_reads_take_format_sys_attrs_and_order_by() {
    let schema = |name: &str| {
        TOOLS
            .iter()
            .find(|tool| tool.name == name)
            .expect("the tool")
            .input_schema()
    };
    for name in ["get_entity", "query_entities"] {
        let properties = &schema(name)["properties"];
        assert_eq!(
            properties["format"]["enum"],
            json!(["normalized", "concise", "keyValues"]),
            "{name}"
        );
        assert_eq!(properties["sysAttrs"]["type"], "boolean", "{name}");
    }
    for name in ["query_temporal", "retrieve_temporal"] {
        let properties = &schema(name)["properties"];
        assert_eq!(
            properties["format"]["enum"],
            json!(["normalized", "temporalValues", "aggregatedValues"]),
            "{name}"
        );
        assert_eq!(properties["sysAttrs"]["type"], "boolean", "{name}");
    }
    assert_eq!(
        schema("query_entities")["properties"]["orderBy"]["type"],
        "string"
    );
    assert!(schema("batch_query_temporal")["properties"]
        .get("format")
        .is_none());
    let query = schema("query_entities");
    for argument in ["q", "idPattern", "scopeQ", "georel", "orderBy", "format"] {
        assert!(
            query["properties"][argument]["description"].is_string(),
            "{argument} carries no grammar"
        );
    }
    assert!(
        schema("query_temporal")["properties"]["timerel"]["description"]
            .as_str()
            .is_some_and(|line| line.contains("between"))
    );
}

/// EP-116: the hub and every named server say they speak NGSI-LD and teach its language, and
/// the primer names nothing a caller could use to reach around the hub.
#[test]
fn the_primer_teaches_ngsi_ld_and_names_no_slug_token_or_host() {
    let primer: &str = &crate::mcp::PRIMER;
    for word in [
        "NGSI-LD",
        "@context",
        "`q`",
        "idPattern",
        "timerel",
        "scopeQ",
        "orderBy",
        "sysAttrs",
        "lastN",
        "aggrMethods",
        "deletedAt",
        "list_endpoints",
        "describe_schema",
        "DCAT-AP",
    ] {
        assert!(primer.contains(word), "the primer does not mention {word}");
    }
    assert!(primer.split_whitespace().count() < 600);
    for leak in [
        "http://",
        "https://",
        ".svc",
        "Bearer",
        "joinedcontext.com",
        "/api/",
    ] {
        assert!(!primer.contains(leak), "the primer carries {leak}");
    }
    let named = crate::mcp::server::initialized(None);
    assert!(named["instructions"]
        .as_str()
        .is_some_and(|text| text.ends_with(primer)));
}

/// The broker's types: `type` of a query is a list, a cursor is text, a single entity's id
/// is the URN (Antares `schema.rs`), so the published schema says what the view takes.
#[test]
fn a_published_schema_has_the_broker_s_types() {
    let schema = |name: &str| {
        TOOLS
            .iter()
            .find(|tool| tool.name == name)
            .expect("the tool")
            .input_schema()
    };
    let query = schema("query_entities");
    assert_eq!(query["properties"]["type"]["type"], "array");
    assert_eq!(query["properties"]["cursor"]["type"], "string");
    assert_eq!(query["additionalProperties"], false);
    assert!(query.get("required").is_none());
    let batch = schema("batch_query");
    assert_eq!(batch["required"], serde_json::json!(["type"]));
    let upsert = schema("batch_upsert");
    assert_eq!(
        upsert["properties"]["entities"]["items"]["required"],
        serde_json::json!(["id", "type"])
    );
    let subscribe = schema("create_subscription");
    assert_eq!(
        subscribe["properties"]["subscription"]["required"],
        serde_json::json!(["type", "notification"])
    );
    for tool in TOOLS {
        assert!(
            jsonschema::draft7::new(&tool.input_schema()).is_ok(),
            "{} publishes a schema that does not compile",
            tool.name
        );
    }
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
        shape: Shape::Own(|| serde_json::json!({})),
        result_key: "entities",
    };
    assert!(!mixed.read_only());
}
