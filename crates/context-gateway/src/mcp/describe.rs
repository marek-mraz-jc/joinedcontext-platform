//! The two tools that describe an Endpoint rather than its data, `describe_access` and
//! `describe_schema`, and the `access://` and `schema://` resources behind them (EP-46, EP-47,
//! EP-55 to EP-60). The broker has no tool or resource for either (Antares `mcp-surface.md`
//! section 9), so the gateway answers them from the same projections its access and schema
//! surfaces serve, and adds them to the broker's listings (T-3647, T-3648).

use crate::app::{sha256_hex, Gateway};
use crate::handlers::view_access::{self, AccessFormat};
use crate::handlers::{schema, schema_routes};
use crate::mcp::jsonrpc::{answered, error, refused, result};
use crate::mcp::tools::{granted, validate, ACCESS_FORMATS, TOOLS};
use crate::pdp::evaluator::Subject;
use crate::resolver::{Endpoint, Model};
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
                "annotations": { "readOnlyHint": true, "destructiveHint": false },
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
    match describe_schema_for(gateway, endpoint, subject, &arguments).await {
        Ok(document) => result(id, answered(&document, tool.result_key)),
        Err(message) => result(id, refused(&message, &Value::Null)),
    }
}

/// The access document and every schema artifact this caller may read, as `resources/list`
/// lists them, so a client that attaches resources rather than calling tools reaches the same
/// documents (EP-46, T-1858).
pub(crate) fn resources(endpoint: &Endpoint, subject: &Subject) -> Vec<Value> {
    let mut listed = vec![json!({
        "uri": format!("access://{}", endpoint.slug),
        "name": "access",
        "description": "The caller's effective grants on this endpoint.",
        "mimeType": "application/json",
    })];
    let visible = schema::visible(subject, endpoint, crate::pdp::now());
    for model in &endpoint.models {
        // The cube is listed only where this caller's projection of the model holds a Data
        // Structure Definition, as the REST index lists it (DM-60).
        for artifact in schema::published(&[model], &visible) {
            listed.push(json!({
                "uri": schema_uri(&endpoint.slug, model.major, artifact),
                "name": format!("{} v{} {}", model.name, model.major, artifact.format_name()),
                "description": "A rendered schema artifact of this space's data model.",
                "mimeType": artifact.media_type(),
            }));
        }
    }
    listed
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
            match describe_schema_for(gateway, endpoint, subject, &asked).await {
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
    schema::Artifact::from_format(artifact)
        .or_else(|| schema::artifact_of(artifact, ""))
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
const RECOMMENDED: schema::Artifact = schema::Artifact::LinkMl;

/// The entity types a `describe_schema` call narrows to: one name, or the list of classes an
/// agent needs (T-1858). Absent means every type the caller may be described.
fn narrowed_to<'a>(
    visible: &schema::Visible,
    argument: Option<&'a Value>,
) -> Result<schema::Visible, String> {
    let wanted: Vec<&'a str> = match argument {
        None => return Ok(visible.clone()),
        Some(Value::String(one)) => vec![one.as_str()],
        Some(Value::Array(many)) => many.iter().filter_map(Value::as_str).collect(),
        Some(_) => return Err("`entityType` is one entity type or a list of them".to_owned()),
    };
    visible.only_each(wanted).map_err(|unknown| {
        format!("`{unknown}` is not an entity type this endpoint describes for you")
    })
}

/// The catalogue of what this endpoint describes, as an agent reads it (EP-46, T-1858).
///
/// The models and their digests are the ones the REST index publishes, so the two doors
/// cannot disagree about what exists; what is added here is the reading order. `recommended`
/// names the one formalism to load first, and every artifact is listed with the `format` that
/// asks for it, its media type, its size, the digest of the very bytes a fetch returns, and
/// the `schema://` URI that attaches it as a resource.
///
/// The artifacts are listed per model major rather than per model, because the major is what
/// a fetch addresses: two models of one major render into one document, and a digest taken
/// per model would then be the digest of nothing a caller can ask for.
fn schema_summary(endpoint: &Endpoint, visible: &schema::Visible) -> Value {
    // The very digest function the REST index is built with: the two doors publish one
    // catalogue of one set of documents, so a client may cache on either (EP-46, SP-16).
    let digest = |body: &[u8]| sha256_hex(body);
    let document = schema::index(endpoint, visible, digest);

    let mut majors: Vec<u32> = endpoint.models.iter().map(|model| model.major).collect();
    majors.sort_unstable();
    majors.dedup();

    let mut listed = Vec::new();
    for major in majors {
        let models: Vec<&Model> = endpoint
            .models
            .iter()
            .filter(|model| model.major == major)
            .collect();
        for (artifact, bytes, sha256) in schema::measured(&models, visible, &digest) {
            listed.push(json!({
                "format": artifact.format_name(),
                "mediaType": artifact.media_type(),
                "version": major,
                "bytes": bytes,
                "sha256": sha256,
                "uri": schema_uri(&endpoint.slug, major, artifact),
            }));
        }
    }

    recommended(document, listed)
}

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

/// The `resources/read` URI of one artifact of one major.
fn schema_uri(slug: &str, major: u32, artifact: schema::Artifact) -> String {
    format!("schema://{slug}/v{major}/{}", artifact.format_name())
}

/// What a call for the cube of a model with no Data Structure Definition is told.
const NO_CUBE: &str =
    "this endpoint's model declares no Data Structure Definition you may read, so \
     it has no RDF Data Cube; ask for shacl, owl or rdf instead";

/// `describe_schema` and the `schema://` resources (EP-46, EP-47): from the broker's LinkML
/// surface of the Endpoint's view when one serves it (T-3645), so an agent reads the very bytes
/// and digests the REST `schema/` routes serve. A call narrowed with `entityType` has no REST
/// counterpart and is still rendered here, and so is an Endpoint no view serves.
async fn describe_schema_for(
    gateway: &Gateway,
    endpoint: &Endpoint,
    subject: &Subject,
    arguments: &Map<String, Value>,
) -> Result<Value, String> {
    if !arguments.contains_key("entityType") {
        if let Some(answer) = schema_from_view(gateway, endpoint, subject, arguments).await {
            return answer;
        }
    }
    describe_schema(endpoint, subject, arguments)
}

/// [`describe_schema`] answered from the broker's documents; `None` hands the call back (no
/// view, or a format name only the local answer explains).
async fn schema_from_view(
    gateway: &Gateway,
    endpoint: &Endpoint,
    subject: &Subject,
    arguments: &Map<String, Value>,
) -> Option<Result<Value, String>> {
    let unavailable = |status: StatusCode| {
        tracing::warn!(slug = %endpoint.slug, %status, "the broker's schema surface did not answer");
        "the data model cannot be read right now; try again later".to_owned()
    };
    let format = arguments
        .get("format")
        .and_then(Value::as_str)
        .unwrap_or("summary");
    let artifact = schema::Artifact::from_format(format);
    if format != "summary" && artifact.is_none() {
        return None;
    }
    let index = match schema_routes::view_document(gateway, endpoint, subject, "index.json").await?
    {
        Ok(bytes) => serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null),
        Err(status) => return Some(Err(unavailable(status))),
    };
    let majors: Vec<u64> = index["models"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(schema_routes::major_of)
        .collect();
    let Some(artifact) = artifact else {
        return Some(Ok(view_summary(&endpoint.slug, index)));
    };
    let wanted = arguments.get("version").and_then(Value::as_u64);
    let Some(major) = wanted
        .or_else(|| majors.first().copied())
        .filter(|m| majors.contains(m))
    else {
        return Some(Err(
            "this endpoint publishes no model of that version".to_owned()
        ));
    };
    let path = format!("v{major}/{}", artifact.file_name());
    let bytes = match schema_routes::view_document(gateway, endpoint, subject, &path).await? {
        Ok(bytes) => bytes,
        Err(StatusCode::NOT_FOUND) if artifact == schema::Artifact::Qb => {
            return Some(Err(NO_CUBE.to_owned()));
        }
        Err(status) => return Some(Err(unavailable(status))),
    };
    let rendered = if artifact.is_json() {
        match serde_json::from_slice(&bytes) {
            Ok(document) => document,
            Err(_) => return Some(Err(unavailable(StatusCode::BAD_GATEWAY))),
        }
    } else {
        json!({
            "format": format,
            "mediaType": artifact.media_type(),
            "document": String::from_utf8_lossy(&bytes),
        })
    };
    Some(within_bound(format, &rendered).map(|()| rendered))
}

/// The summary of the broker's catalogue: its models, and every artifact with the `format`
/// that asks for it, its size, digest and `schema://` URI, as [`schema_summary`] lists them.
fn view_summary(slug: &str, index: Value) -> Value {
    let mut listed = Vec::new();
    for model in index["models"].as_array().into_iter().flatten() {
        let Some(major) = schema_routes::major_of(model).and_then(|m| u32::try_from(m).ok()) else {
            continue;
        };
        // LinkML first, in the order the local summary lists them.
        for artifact in schema::Artifact::ALL
            .into_iter()
            .chain([schema::Artifact::Qb])
        {
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

/// The data model, in the formalism asked for and narrowed to the caller's grant (EP-47).
///
/// Every formalism the REST schema surface renders is rendered here from the same projected
/// JSON Schema, so an agent reads exactly what a browser reads for the same token (EP-46,
/// EP-52, T-0845); the text ones come back as `{format, mediaType, document}`.
fn describe_schema(
    endpoint: &Endpoint,
    subject: &Subject,
    arguments: &Map<String, Value>,
) -> Result<Value, String> {
    let visible = schema::visible(subject, endpoint, crate::pdp::now());
    let visible = narrowed_to(&visible, arguments.get("entityType"))?;
    let format = arguments
        .get("format")
        .and_then(Value::as_str)
        .unwrap_or("summary");
    if format == "summary" {
        return Ok(schema_summary(endpoint, &visible));
    }

    let major = arguments.get("version").and_then(Value::as_u64);
    let models: Vec<&Model> = endpoint
        .models
        .iter()
        .filter(|model| major.is_none_or(|wanted| u64::from(model.major) == wanted))
        .collect();
    if models.is_empty() {
        return Err("this endpoint publishes no model of that version".to_owned());
    }

    let Some(artifact) = schema::Artifact::from_format(format) else {
        return Err(format!(
            "`{format}` is not a formalism this endpoint renders: it serves summary, \
             json-schema, context, linkml, shacl, owl, rdf, markdown, and qb where the model \
             declares a Data Structure Definition"
        ));
    };
    if artifact == schema::Artifact::Qb && !schema::declares_dsd(&models, &visible) {
        return Err(NO_CUBE.to_owned());
    }

    let mut redacted = Vec::new();
    let rendered = match artifact {
        // The two JSON formalisms answer as the object they are; the five text ones as
        // `{format, mediaType, document}`, rendered the way the REST route renders them.
        // `Accept` plays no part here, the name of the format decides.
        schema::Artifact::JsonSchema => schema::json_schema(&models, &visible, &mut redacted),
        schema::Artifact::Context => schema::context(&models, &visible, &mut redacted),
        other => json!({
            "format": format,
            "mediaType": other.media_type(),
            "document": schema::render(&models, other, &visible),
        }),
    };
    within_bound(format, &rendered)?;
    Ok(rendered)
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
