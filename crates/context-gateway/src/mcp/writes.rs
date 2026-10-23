//! A write tool's arguments under the guards the NGSI-LD surface runs on the same write
//! (T-4590): a tool is the NGSI-LD request it stands for, so its body argument passes the core
//! context check, a subscription's notification target check, a form's minted id and one entity
//! per create, the model's units and relationships, and a form's daily create quota, exactly as
//! that request would at `/ngsi-ld/v1`. What the guards change (a minted id, stripped
//! quarantine marks) goes on to the broker in place of what the caller sent.

use crate::app::{Gateway, MAX_BODY};
use crate::auth::subject::principal_of;
use crate::handlers::ngsi_ld::{reserve_create, view_payload};
use crate::pdp::evaluator::Subject;
use crate::resolver::Endpoint;
use crate::{mcp, operations, query};
use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderMap, HeaderValue, Method};
use serde_json::{Map, Value};

/// A create the call holds of a form's daily quota, released when the broker does not take it.
pub(crate) struct Reserved {
    caller: String,
    at: u64,
}

impl Reserved {
    pub(crate) fn release(self, gateway: &Gateway, endpoint: &Endpoint) {
        gateway
            .creates
            .release(&endpoint.slug, &self.caller, self.at);
    }
}

/// Runs the guards on a `tools/call` of a write tool and rewrites its body argument with what
/// they kept; a read tool passes untouched. `Err` is the JSON-RPC answer that refuses the call.
pub(crate) async fn guarded(
    gateway: &Gateway,
    endpoint: &Endpoint,
    subject: &Subject,
    caller: &str,
    message: &mut Value,
) -> Result<Option<Reserved>, Value> {
    let id = message["id"].clone();
    // A call that names no tool runs none: the broker answers it with its invalid-params error.
    let Some(tool) = message["params"]["name"].as_str().map(str::to_owned) else {
        return Ok(None);
    };
    if is_read_tool(&tool) {
        return Ok(None);
    }
    let refuse =
        |why: &str| mcp::jsonrpc::result(id.clone(), mcp::jsonrpc::refused(why, &Value::Null));
    let empty = Map::new();
    let arguments = message["params"]["arguments"].as_object().unwrap_or(&empty);
    // A write the gateway cannot name as an NGSI-LD request is one it cannot guard: refused,
    // as an unknown tool is to an anonymous caller.
    let Some((method, path, body)) = request_of(&tool, arguments) else {
        tracing::info!(slug = %endpoint.slug, %tool, "MCP write refused: no NGSI-LD request it stands for");
        return Err(refuse(
            "this write tool is not offered here, or its arguments are incomplete",
        ));
    };
    let Some(operation) = operations::operation_of(&method, &path, false) else {
        return Err(refuse("this write names no resource the Endpoint serves"));
    };
    let mut sent = match body.and_then(|name| arguments.get(name)) {
        Some(value) => serde_json::to_vec(value).unwrap_or_default(),
        None => Vec::new(),
    };
    // The OData surface refuses a write to an attribute the Endpoint hides; a tool is held to the
    // same, by every name its body or its path gives.
    let hidden = body
        .and_then(|name| arguments.get(name))
        .into_iter()
        .chain(
            arguments
                .get("entities")
                .and_then(Value::as_array)
                .into_iter()
                .flatten(),
        )
        .filter_map(Value::as_object)
        .flat_map(|members| members.keys().map(String::as_str))
        .chain(word(arguments, "attrId"))
        .find(|name| endpoint.hidden_attributes.contains(*name));
    if let Some(name) = hidden {
        tracing::info!(slug = %endpoint.slug, %tool, "MCP write refused: a hidden attribute");
        return Err(refuse(&format!(
            "`{name}` is not an attribute this Endpoint lets you write"
        )));
    }
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    // The relationship guards read and unlink in the Endpoint's space, as the NGSI-LD surface's
    // pinned request does; without it they ran in the broker's default tenant (T-4755).
    let Ok(space) = HeaderValue::from_str(&endpoint.space) else {
        return Err(refuse("this Endpoint names an unusable space"));
    };
    headers.insert(crate::middleware::tenancy::TENANT, space);
    let mut divided = None;
    if let Err(refusal) = view_payload(
        gateway,
        (endpoint, subject),
        (&path, &[], operation),
        &mut headers,
        &mut sent,
        &mut divided,
    )
    .await
    {
        let held = axum::body::to_bytes(refusal.into_body(), MAX_BODY)
            .await
            .unwrap_or_default();
        tracing::info!(slug = %endpoint.slug, %tool, principal = %principal_of(subject), "MCP write refused by the Endpoint's write guards");
        return Err(refuse(&refusal_text(&held)));
    }
    // A batch the NGSI-LD surface would split (207) is refused whole: a tool call has one result.
    if let Some((_, refused)) = divided {
        let (first, problem) = &refused[0];
        let why = problem.detail.as_deref().unwrap_or(problem.title.as_str());
        return Err(refuse(&format!(
            "{} of the entities were refused, {first} first: {why}; send the others without them",
            refused.len()
        )));
    }
    if let (Some(name), Ok(kept)) = (body, serde_json::from_slice::<Value>(&sent)) {
        if let Some(arguments) = message["params"]["arguments"].as_object_mut() {
            arguments.insert(name.to_owned(), kept);
        }
    }
    match reserve_create(gateway, endpoint, caller, operation) {
        Ok(at) => Ok(at.map(|at| Reserved {
            caller: caller.to_owned(),
            at,
        })),
        Err(refusal) => {
            let held = axum::body::to_bytes(refusal.into_body(), MAX_BODY)
                .await
                .unwrap_or_default();
            Err(refuse(&refusal_text(&held)))
        }
    }
}

fn word<'a>(arguments: &'a Map<String, Value>, name: &str) -> Option<&'a str> {
    arguments.get(name).and_then(Value::as_str)
}

/// The sentence of a Problem Details body, or a plain refusal when it has none.
fn refusal_text(body: &[u8]) -> String {
    problem_text(body).unwrap_or_else(|| "this Endpoint refused the write".to_owned())
}

/// The NGSI-LD request a broker write tool stands for (Antares `docs/src/mcp.md` section 2): its
/// method, its path under `/ngsi-ld/v1` and the argument that is its body. `None` for a tool
/// this table does not know, or one whose path argument is missing.
fn request_of(
    tool: &str,
    arguments: &Map<String, Value>,
) -> Option<(Method, String, Option<&'static str>)> {
    let (_, method, template, body) = REQUESTS.iter().find(|(name, ..)| *name == tool)?;
    let mut path = (*template).to_owned();
    if path.contains("{id}") {
        path = path.replace("{id}", &query::encode(word(arguments, "id")?));
    }
    if path.contains("{attrId}") {
        path = path.replace("{attrId}", &query::encode(word(arguments, "attrId")?));
    }
    Some((method.clone(), path, *body))
}

/// Tool, method, path (`{id}` percent-encoded, `{attrId}` as given) and body argument.
const REQUESTS: &[(&str, Method, &str, Option<&str>)] = &[
    ("create_entity", Method::POST, "/entities", Some("entity")),
    (
        "replace_entity",
        Method::PUT,
        "/entities/{id}",
        Some("entity"),
    ),
    (
        "merge_entity",
        Method::PATCH,
        "/entities/{id}",
        Some("fragment"),
    ),
    ("delete_entity", Method::DELETE, "/entities/{id}", None),
    (
        "append_attributes",
        Method::POST,
        "/entities/{id}/attrs",
        Some("attributes"),
    ),
    (
        "update_attributes",
        Method::PATCH,
        "/entities/{id}/attrs",
        Some("attributes"),
    ),
    (
        "update_attribute",
        Method::PATCH,
        "/entities/{id}/attrs/{attrId}",
        Some("attribute"),
    ),
    (
        "replace_attribute",
        Method::PUT,
        "/entities/{id}/attrs/{attrId}",
        Some("attribute"),
    ),
    (
        "delete_attribute",
        Method::DELETE,
        "/entities/{id}/attrs/{attrId}",
        None,
    ),
    (
        "batch_create",
        Method::POST,
        "/entityOperations/create",
        Some("entities"),
    ),
    (
        "batch_upsert",
        Method::POST,
        "/entityOperations/upsert",
        Some("entities"),
    ),
    (
        "batch_update",
        Method::POST,
        "/entityOperations/update",
        Some("entities"),
    ),
    (
        "batch_merge",
        Method::POST,
        "/entityOperations/merge",
        Some("entities"),
    ),
    (
        "batch_delete",
        Method::POST,
        "/entityOperations/delete",
        Some("ids"),
    ),
    (
        "create_subscription",
        Method::POST,
        "/subscriptions",
        Some("subscription"),
    ),
    (
        "update_subscription",
        Method::PATCH,
        "/subscriptions/{id}",
        Some("fragment"),
    ),
    (
        "delete_subscription",
        Method::DELETE,
        "/subscriptions/{id}",
        None,
    ),
];

/// The sentence of a Problem Details body: its `detail`, else its `title`.
pub(crate) fn problem_text(body: &[u8]) -> Option<String> {
    let problem = serde_json::from_slice::<Value>(body).ok()?;
    problem["detail"]
        .as_str()
        .or_else(|| problem["title"].as_str())
        .map(str::to_owned)
}

/// Whether the broker tool only reads (see [`MCP_READ_TOOLS`]).
pub(crate) fn is_read_tool(tool: &str) -> bool {
    MCP_READ_TOOLS.contains(&tool)
}

/// The broker's read tools (Antares `docs/src/mcp.md` section 2); any other tool is a write.
const MCP_READ_TOOLS: &[&str] = &[
    "area_overview",
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
    // a profile of what query_entities reads (Antares T-4680)
    "summarize",
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handlers::ngsi_ld::create_key;
    use serde_json::json;

    /// An attribute name stays one path segment: `../subscriptions` cannot turn an attribute
    /// write into a request on another resource the guards would classify instead.
    #[test]
    fn an_attribute_name_cannot_leave_its_segment() {
        let arguments = json!({"id": "urn:ngsi-ld:Thing:a", "attrId": "../../../subscriptions"});
        let (_, path, _) = request_of("update_attribute", arguments.as_object().unwrap()).unwrap();
        assert!(!path.contains("/../"), "{path}");
        assert_eq!(path.matches('/').count(), 4, "{path}");
    }

    /// Every tool the gateway lists is a read or a write it can name as an NGSI-LD request, and
    /// that request is the operation the tool's grant names: a tool added to one list and not the
    /// other fails here, not as an unguarded write.
    #[test]
    fn every_listed_write_tool_is_the_ngsi_ld_request_its_grant_names() {
        let arguments = json!({"id": "urn:ngsi-ld:Thing:a b", "attrId": "speed"});
        let arguments = arguments.as_object().unwrap();
        for tool in mcp::tools::TOOLS
            .iter()
            .filter(|tool| matches!(tool.shape, mcp::tools::Shape::Broker(_)))
        {
            if is_read_tool(tool.name) {
                assert!(request_of(tool.name, arguments).is_none(), "{}", tool.name);
                continue;
            }
            let (method, path, _) = request_of(tool.name, arguments)
                .unwrap_or_else(|| panic!("{} is a write with no NGSI-LD request", tool.name));
            assert_eq!(
                operations::operation_of(&method, &path, false).as_ref(),
                tool.operations.first(),
                "{}",
                tool.name
            );
        }
    }

    #[test]
    fn an_id_is_one_path_segment_and_a_missing_one_names_no_request() {
        let arguments = json!({"id": "urn:ngsi-ld:Thing:x/../../subscriptions"});
        let (_, path, _) = request_of("delete_entity", arguments.as_object().unwrap()).unwrap();
        assert_eq!(
            path,
            "/entities/urn%3Angsi-ld%3AThing%3Ax%2F..%2F..%2Fsubscriptions"
        );
        assert!(request_of("replace_entity", &Map::new()).is_none());
        assert!(request_of("a_tool_added_later", &Map::new()).is_none());
    }

    #[test]
    fn a_signed_in_caller_spends_their_own_creates_and_an_anonymous_one_their_address() {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", HeaderValue::from_static("10.0.0.7"));
        let jana = Subject {
            user: Some("jana".to_owned()),
            ..Subject::default()
        };
        assert_eq!(create_key(&jana, &headers), "principal:user:jana");
        assert_eq!(
            create_key(&Subject::anonymous(), &headers),
            "address:10.0.0.7"
        );
    }
}
