//! The two tools that describe an Endpoint rather than its data, `describe_access` and
//! `describe_schema`, and the `access://` and `schema://` resources behind them (EP-46, EP-47,
//! EP-55 to EP-60). The broker has no tool or resource for either (Antares `mcp-surface.md`
//! section 9), so the gateway answers them from the documents its access and schema surfaces
//! serve, and adds them to the broker's listings (T-3647, T-3648). Schemas are the broker's
//! LinkML surface of the Endpoint's view, read as the REST `schema/` routes read it; the
//! gateway renders none (T-3646, map 7.4).

use crate::app::Gateway;
use crate::handlers::schema_routes;
use crate::handlers::view_access::{self, AccessFormat};
use crate::mcp::jsonrpc::{answered, error, refused, result};
use crate::mcp::tools::{granted, validate, ACCESS_FORMATS, TOOLS};
use crate::middleware::response::{self, Artifact};
use crate::pdp::evaluator::Subject;
use crate::resolver::Endpoint;
use axum::http::StatusCode;
use serde_json::{json, Map, Value};

/// The tools the gateway answers itself.
pub(crate) const DESCRIBE_TOOLS: &[&str] = &["describe_access", "describe_schema"];

/// The resources the gateway serves itself, by URI scheme.
pub(crate) const OWN_SCHEMES: &[&str] = &["schema://", "access://"];

/// The describe tools this caller may call, as `tools/list` lists them (EP-25, SP-15).
pub(crate) fn tools(gateway: &Gateway, endpoint: &Endpoint, subject: &Subject) -> Vec<Value> {
    TOOLS
        .iter()
        .filter(|tool| DESCRIBE_TOOLS.contains(&tool.name))
        .filter(|tool| granted(gateway, endpoint, subject, tool.operations))
        .map(|tool| {
            json!({
                "name": tool.name,
                "description": tool.description,
                "inputSchema": tool.input_schema(),
                "outputSchema": crate::mcp::tools::output_schema(tool),
                "annotations": crate::mcp::tools::annotations(tool.name, true, false),
            })
        })
        .collect()
}

/// One call of a describe tool, answered as the JSON-RPC response it is. Any other name is an
/// unknown tool, and so is a describe tool this caller may not call: the two answers are
/// byte-identical (SP-20).
pub(crate) async fn call(
    gateway: &Gateway,
    endpoint: &Endpoint,
    subject: &Subject,
    id: Value,
    params: &Value,
) -> Value {
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let arguments = match params.get("arguments") {
        None | Some(Value::Null) => Map::new(),
        Some(Value::Object(arguments)) => arguments.clone(),
        Some(_) => return error(id, -32602, "`arguments` is an object"),
    };
    let Some((index, tool)) = TOOLS
        .iter()
        .enumerate()
        .find(|(_, tool)| tool.name == name && DESCRIBE_TOOLS.contains(&tool.name))
    else {
        return error(id, -32602, "unknown tool");
    };
    if !granted(gateway, endpoint, subject, tool.operations) {
        return error(id, -32602, "unknown tool");
    }
    // AG-31: the arguments are checked against the tool's own published schema first.
    if let Err(problem) = validate(index, &arguments) {
        return result(id, refused(&problem, &Value::Null));
    }
    if tool.name == "describe_access" {
        // The schema's enum already refused any other word (AG-31).
        let format = arguments
            .get("format")
            .and_then(Value::as_str)
            .unwrap_or("permissions");
        return match access_document(gateway, subject, endpoint, format).await {
            Ok(document) => result(id, answered(&document, tool.result_key)),
            Err(message) => result(id, refused(&message, &Value::Null)),
        };
    }
    match describe_schema(gateway, endpoint, subject, &arguments).await {
        Ok(document) => result(id, answered(&document, tool.result_key)),
        Err(message) => result(id, refused(&message, &Value::Null)),
    }
}

/// The access document and every schema artifact this caller may read, as `resources/list`
/// lists them, so a client that attaches resources rather than calling tools reaches the same
/// documents (EP-46, T-1858). The artifacts are the ones the broker's index of the Endpoint's
/// view lists for this caller (T-3646).
pub(crate) async fn resources(
    gateway: &Gateway,
    endpoint: &Endpoint,
    subject: &Subject,
) -> Vec<Value> {
    let mut listed = vec![json!({
        "uri": format!("access://{}", endpoint.slug),
        "name": "access",
        "description": "The caller's effective grants on this endpoint.",
        "mimeType": "application/json",
    })];
    let index = schema_routes::index_for(gateway, endpoint, &endpoint.slug, subject).await;
    for model in index["models"].as_array().into_iter().flatten() {
        let Some(major) = schema_routes::major_of(model).and_then(|m| u32::try_from(m).ok()) else {
            continue;
        };
        let name = model["name"].as_str().unwrap_or_default();
        for artifact in listed_artifacts(model) {
            listed.push(json!({
                "uri": schema_uri(&endpoint.slug, major, artifact),
                "name": format!("{name} v{major} {}", artifact.format_name()),
                "description": "A rendered schema artifact of this space's data model.",
                "mimeType": artifact.media_type(),
            }));
        }
    }
    listed
}

/// The artifacts one model of the broker's index lists, LinkML first.
fn listed_artifacts(model: &Value) -> Vec<Artifact> {
    Artifact::ALL
        .into_iter()
        .chain([Artifact::Qb])
        .filter(|artifact| !model["artifacts"][artifact.file_name()].is_null())
        .collect()
}

/// One `resources/read` of an `access://` or `schema://` URI. A URI this Endpoint does not serve,
/// and one naming another Endpoint, answer the same way: the caller learns nothing about what
/// exists elsewhere (AG-05, SP-14, SP-20).
pub(crate) async fn read(
    gateway: &Gateway,
    endpoint: &Endpoint,
    subject: &Subject,
    id: Value,
    params: &Value,
) -> Value {
    let uri = params.get("uri").and_then(Value::as_str).unwrap_or("");
    match parse_resource(endpoint, uri) {
        Some(Resource::Access(format)) => {
            match access_document(gateway, subject, endpoint, &format).await {
                Ok(document) => result(id, contents(uri, &document)),
                Err(message) => result(id, refused(&message, &Value::Null)),
            }
        }
        Some(Resource::Schema { major, format }) => {
            let mut asked = Map::new();
            asked.insert("format".to_owned(), Value::String(format));
            asked.insert("version".to_owned(), Value::from(major));
            match describe_schema(gateway, endpoint, subject, &asked).await {
                Ok(document) => result(id, contents(uri, &document)),
                Err(message) => result(id, refused(&message, &Value::Null)),
            }
        }
        None => error(id, -32602, "unknown resource"),
    }
}

/// The two resource shapes the gateway serves.
enum Resource {
    /// `access://{endpointSlug}`, optionally `?format=odrl|grant-ast|permissions`
    Access(String),
    /// `schema://{endpointSlug}/v{major}/{artifact}`
    Schema { major: u64, format: String },
}

/// Names the resource a URI addresses, or `None` when this Endpoint does not serve it: the slug
/// in the URI has to be this Endpoint's own (AG-05, SP-14).
fn parse_resource(endpoint: &Endpoint, uri: &str) -> Option<Resource> {
    if let Some(rest) = uri.strip_prefix("access://") {
        let (slug, format) = match rest.split_once("?format=") {
            Some((slug, format)) => (slug, format),
            None => (rest, "permissions"),
        };
        return (slug == endpoint.slug && ACCESS_FORMATS.contains(&format))
            .then(|| Resource::Access(format.to_owned()));
    }
    let rest = uri.strip_prefix("schema://")?;
    let (slug, path) = rest.split_once('/')?;
    if slug != endpoint.slug {
        return None;
    }
    let (version, artifact) = path.split_once('/')?;
    let major = version.strip_prefix('v')?.parse().ok()?;
    // The `format` name the summary publishes, and the REST file name for the same document:
    // one artifact, whichever spelling the client copied (EP-52).
    Artifact::from_format(artifact)
        .or_else(|| response::artifact_of(artifact, ""))
        .map(|artifact| Resource::Schema {
            major,
            format: artifact.format_name().to_owned(),
        })
}

/// The caller's grants in one of [`ACCESS_FORMATS`], read by the very function the HTTP access
/// surface serves, so MCP and HTTP answer the same document (EP-60, T-0425, T-3649). A broker
/// failure is the text a tool error carries, naming nothing the broker said.
async fn access_document(
    gateway: &Gateway,
    subject: &Subject,
    endpoint: &Endpoint,
    format: &str,
) -> Result<Value, String> {
    let format = match format {
        "odrl" => AccessFormat::Odrl,
        "grant-ast" => AccessFormat::GrantAst,
        _ => AccessFormat::AuthZen,
    };
    view_access::document(gateway, endpoint, subject, format)
        .await
        .map_err(|answer| {
            format!(
                "the access document could not be read (HTTP {}); try again in a moment",
                answer.status().as_u16()
            )
        })
}

/// One resource, in the envelope `resources/read` answers with.
fn contents(uri: &str, payload: &Value) -> Value {
    json!({
        "contents": [{
            "uri": uri,
            "mimeType": "application/json",
            "text": serde_json::to_string(payload).unwrap_or_else(|_| "null".to_owned()),
        }],
    })
}

/// The largest document one tool answer carries (T-1858).
///
/// An MCP answer is read into a model's context window, so a schema of several megabytes
/// costs the caller its whole window and is of no use to it. A document over this is refused
/// with the way to narrow it rather than cut: half a SHACL file is not SHACL, and a model
/// that silently lost its last classes is the one leak a schema surface must not have.
const DOCUMENT_BOUND: usize = 1024 * 1024;

/// The formalism an agent is told to read before the others.
///
/// LinkML carries the classes, the slots, their ranges and their descriptions in one small
/// YAML document; the Turtle renderings say the same in more bytes, and the JSON Schema says
/// less about the meaning of a slot.
const RECOMMENDED: Artifact = Artifact::LinkMl;

/// The catalogue with its per-model copies of the digests taken out and the reading order and
/// the flat artifact list put in.
fn recommended(mut document: Value, listed: Vec<Value>) -> Value {
    // The per-model copy of the same digests, under a second name and in file-name spelling,
    // would only be a second thing to keep true.
    if let Some(models) = document["models"].as_array_mut() {
        for model in models {
            if let Value::Object(described) = model {
                described.remove("artifacts");
            }
        }
    }
    document["recommended"] = json!(RECOMMENDED.format_name());
    document["recommendedBecause"] = json!(
        "LinkML carries the classes, their slots, ranges and descriptions in one small \
         document; read it first and ask for another formalism only when a tool needs it."
    );
    document["artifacts"] = Value::Array(listed);
    document
}

/// `describe_schema` and the `schema://` resources (EP-46, EP-47), from the broker's LinkML
/// surface of the Endpoint's view (T-3645), so an agent reads the very bytes and digests the
/// REST `schema/` routes serve. Nothing is rendered here (T-3646): an Endpoint no view serves
/// describes no model.
async fn describe_schema(
    gateway: &Gateway,
    endpoint: &Endpoint,
    subject: &Subject,
    arguments: &Map<String, Value>,
) -> Result<Value, String> {
    let view = endpoint.slug.as_str();
    let unavailable = |status: StatusCode| {
        tracing::warn!(slug = %endpoint.slug, %status, "the broker's schema surface did not answer");
        "the data model cannot be read right now; try again later".to_owned()
    };
    let read = |rest: String| async move {
        schema_routes::view_document(gateway, endpoint, view, subject, &rest)
            .await
            .unwrap_or(Err(StatusCode::NOT_FOUND))
    };
    let format = arguments
        .get("format")
        .and_then(Value::as_str)
        .unwrap_or("summary");
    if let Some(wanted) = arguments.get("entityType") {
        return classes_of(endpoint, format, wanted, read).await;
    }
    let Some(artifact) = Artifact::from_format(format) else {
        if format != "summary" {
            return Err(format!(
                "`{format}` is not a formalism this endpoint renders: it serves summary, \
                 json-schema, context, linkml, shacl, owl, rdf, markdown, and qb where the model \
                 declares a Data Structure Definition"
            ));
        }
        return match read("schema/index.json".to_owned()).await {
            Ok(bytes) => match serde_json::from_slice::<Value>(&bytes) {
                Ok(index) => Ok(view_summary(&endpoint.slug, index)),
                Err(_) => Err(unavailable(StatusCode::BAD_GATEWAY)),
            },
            Err(StatusCode::NOT_FOUND) => Err(NO_MODEL.to_owned()),
            Err(status) => Err(unavailable(status)),
        };
    };
    let index = match read("schema/index.json".to_owned()).await {
        Ok(bytes) => serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null),
        Err(StatusCode::NOT_FOUND) => return Err(NO_MODEL.to_owned()),
        Err(status) => return Err(unavailable(status)),
    };
    let majors: Vec<u64> = index["models"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(schema_routes::major_of)
        .collect();
    let wanted = arguments.get("version").and_then(Value::as_u64);
    let Some(major) = wanted
        .or_else(|| majors.first().copied())
        .filter(|m| majors.contains(m))
    else {
        return Err("this endpoint publishes no model of that version".to_owned());
    };
    let bytes = match read(format!("schema/v{major}/{}", artifact.file_name())).await {
        Ok(bytes) => bytes,
        Err(StatusCode::NOT_FOUND) if artifact == Artifact::Qb => return Err(NO_CUBE.to_owned()),
        Err(status) => return Err(unavailable(status)),
    };
    let rendered = if artifact.is_json() {
        serde_json::from_slice(&bytes).map_err(|_| unavailable(StatusCode::BAD_GATEWAY))?
    } else {
        json!({
            "format": format,
            "mediaType": artifact.media_type(),
            "document": String::from_utf8_lossy(&bytes),
        })
    };
    within_bound(format, &rendered)?;
    Ok(rendered)
}

/// A `describe_schema` narrowed with `entityType`: the broker's description of each class
/// (`linkml/classes/{type}`: its slots with their ranges and descriptions, narrowed to the
/// caller), which is what an agent with a model too large for one answer reads (T-1858).
///
/// Decided (worker-6, T-3646, owner may override): the broker renders no formalism for one
/// class, so a narrowed call answers the LinkML class descriptions, and a narrowed call for any
/// other formalism is refused with the call that serves it. A type the caller may not read and
/// one that does not exist answer the same words (EP-47, SP-15).
async fn classes_of<F, R>(
    endpoint: &Endpoint,
    format: &str,
    wanted: &Value,
    read: F,
) -> Result<Value, String>
where
    F: Fn(String) -> R,
    R: std::future::Future<Output = Result<Vec<u8>, StatusCode>>,
{
    let names: Vec<&str> = match wanted {
        Value::String(one) => vec![one.as_str()],
        Value::Array(many) => many.iter().filter_map(Value::as_str).collect(),
        _ => return Err("`entityType` is one entity type or a list of them".to_owned()),
    };
    if !matches!(format, "summary" | "linkml") {
        return Err(format!(
            "`entityType` narrows the LinkML description of a class; ask for {format} \
             without `entityType`, or for linkml with it"
        ));
    }
    let mut classes = Vec::new();
    for name in names {
        let segment = crate::query::encode(name);
        let described = match read(format!("classes/{segment}")).await {
            Ok(bytes) => serde_json::from_slice::<Value>(&bytes).ok(),
            Err(StatusCode::NOT_FOUND) => None,
            Err(status) => {
                tracing::warn!(slug = %endpoint.slug, %status, "the broker's class description did not answer");
                return Err("the data model cannot be read right now; try again later".to_owned());
            }
        };
        let Some(described) = described else {
            return Err(format!(
                "`{name}` is not an entity type this endpoint describes for you"
            ));
        };
        classes.push(described);
    }
    let rendered = json!({ "format": "linkml", "classes": classes });
    within_bound("linkml", &rendered)?;
    Ok(rendered)
}

/// Why an Endpoint describes no model: no view serves it, or its Tenant has no schema.
const NO_MODEL: &str = "this endpoint publishes no data model you may read";

/// The summary of the broker's catalogue: its models, and every artifact with the `format`
/// that asks for it, its size, digest and `schema://` URI, LinkML first.
fn view_summary(slug: &str, index: Value) -> Value {
    let mut listed = Vec::new();
    for model in index["models"].as_array().into_iter().flatten() {
        let Some(major) = schema_routes::major_of(model).and_then(|m| u32::try_from(m).ok()) else {
            continue;
        };
        // LinkML first, in the order the local summary lists them.
        for artifact in Artifact::ALL.into_iter().chain([Artifact::Qb]) {
            let described = &model["artifacts"][artifact.file_name()];
            if described.is_null() {
                continue;
            }
            listed.push(json!({
                "format": artifact.format_name(),
                "mediaType": artifact.media_type(),
                "version": major,
                "bytes": described["bytes"],
                "sha256": described["sha256"],
                "uri": schema_uri(slug, major, artifact),
            }));
        }
    }
    recommended(index, listed)
}

/// Why there is no `qb` document (DM-60).
const NO_CUBE: &str = "this endpoint's model declares no Data Structure Definition you may \
                       read, so it has no RDF Data Cube; ask for shacl, owl or rdf instead";

/// The `resources/read` URI of one artifact of one major.
fn schema_uri(slug: &str, major: u32, artifact: Artifact) -> String {
    format!("schema://{slug}/v{major}/{}", artifact.format_name())
}

/// A document too large for one answer is refused with the way to make it smaller, never cut
/// down to the bound (T-1858).
fn within_bound(format: &str, rendered: &Value) -> Result<(), String> {
    let bytes = match rendered.get("document").and_then(Value::as_str) {
        Some(document) => document.len(),
        None => serde_json::to_vec(rendered)
            .map(|body| body.len())
            .unwrap_or(0),
    };
    if bytes <= DOCUMENT_BOUND {
        return Ok(());
    }
    Err(format!(
        "the {format} document of this model is {bytes} bytes, over the {DOCUMENT_BOUND} one \
         answer carries: ask for the classes you need with `entityType` (one name or a list \
         of them), or for one model major with `version`"
    ))
}
