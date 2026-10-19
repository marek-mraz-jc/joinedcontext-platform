//! The standard data tools (T-4748): one of each read-only tool, the Endpoint an argument, the
//! same for one connector and for fifty; and the check every call passes before it is sent.
//! The agent loop and the data loop both offer them.

use serde_json::{json, Value};

use super::agent::Connected;
use super::mcp::{Tool, READ_ONLY_TOOLS};

/// The arguments whose `enum` lists one Endpoint's own names, its types and attributes: the
/// standard tools leave them open, so they are the same for one Endpoint and for fifty (T-4748).
/// Each call is still checked against its Endpoint's own schema, enums included.
const OWN_NAMES: &[&str] = &["type", "attrs", "pick", "omit", "attrId", "geoproperty"];

/// The argument of every data tool that names the Endpoint it reads (T-4748).
pub const ENDPOINT: &str = "endpoint";

/// The rows an unfiltered `query_entities` of the loop answers (T-4748): a sample with the total.
/// A list is asked for with a filter, whose `limit` stands.
pub const SAMPLE_ROWS: u64 = 5;

/// The arguments that make a `query_entities` a filtered read rather than a sample.
const FILTERS: &[&str] = &["q", "id", "idPattern", "georel", "scopeQ"];

/// The data tools the model is offered (T-4748): one of each read-only tool a connector lists,
/// in [`READ_ONLY_TOOLS`] order, with [`ENDPOINT`] required. They used to be one copy per tool
/// and Endpoint, about 550 definitions and 145k tokens for a deployment of 50 connectors.
pub fn standard(connectors: &[Connected]) -> Vec<Value> {
    READ_ONLY_TOOLS
        .iter()
        .filter_map(|name| {
            let tool = connectors
                .iter()
                .flat_map(|c| &c.tools)
                .find(|t| t.name == *name)?;
            Some(json!({"type": "function", "function": {
                "name": tool.name,
                "description": general_description(&tool.description),
                "parameters": general_schema(&tool.input_schema),
            }}))
        })
        .collect()
}

/// A tool's description without its Endpoint's per-type query guide, which the gateway appends.
fn general_description(description: &str) -> &str {
    description
        .split("\n\nQuery guide")
        .next()
        .unwrap_or_default()
        .trim()
}

/// `schema` without its Endpoint's own names and with [`ENDPOINT`] its first required argument.
fn general_schema(schema: &Value) -> Value {
    let mut schema = schema.clone();
    if !schema.is_object() {
        schema = json!({"type": "object"});
    }
    let properties = schema
        .as_object_mut()
        .map(|s| s.entry("properties").or_insert_with(|| json!({})));
    if let Some(Value::Object(properties)) = properties {
        for name in OWN_NAMES {
            if let Some(Value::Object(property)) = properties.get_mut(*name) {
                property.remove("enum");
                if let Some(Value::Object(items)) = property.get_mut("items") {
                    items.remove("enum");
                }
            }
        }
        properties.insert(
            ENDPOINT.into(),
            json!({"type": "string", "minLength": 1, "maxLength": 200, "description": "The Endpoint to read, as the list of Endpoints names it"}),
        );
    }
    let mut required = vec![json!(ENDPOINT)];
    if let Some(Value::Array(own)) = schema.get("required") {
        required.extend(own.iter().filter(|r| *r != ENDPOINT).cloned());
    }
    schema["required"] = json!(required);
    schema
}

/// The connector `named` names (T-4748): its Endpoint name, the name without its `{project}/`,
/// its slug, or its title without case, when exactly one connector has it. Only the
/// deployment's connectors are searched, so a name never reaches an Endpoint they do not.
pub fn endpoint_of<'a>(connectors: &'a [Connected], named: &str) -> Result<&'a Connected, String> {
    let wanted = named.trim();
    if let Some(c) = connectors
        .iter()
        .find(|c| c.endpoint == wanted || c.surface.slug == wanted)
    {
        return Ok(c);
    }
    let lower = wanted.to_lowercase();
    let short = |endpoint: &str| endpoint.rsplit('/').next().unwrap_or(endpoint).to_owned();
    let sharing: Vec<&Connected> = connectors
        .iter()
        .filter(|c| {
            short(&c.endpoint) == short(wanted)
                || c.title
                    .as_deref()
                    .is_some_and(|t| t.trim().to_lowercase() == lower)
        })
        .collect();
    match sharing.as_slice() {
        [one] => Ok(one),
        [] => Err(format!(
            "no Endpoint of this assistant is called `{}`; name one of: {}",
            quoted(wanted),
            names_of(connectors.iter())
        )),
        many => Err(format!(
            "`{}` names several Endpoints; name one of: {}",
            quoted(wanted),
            names_of(many.iter().copied())
        )),
    }
}

/// At most ten Endpoint names, for a refusal the model corrects its call with.
fn names_of<'a>(connectors: impl Iterator<Item = &'a Connected>) -> String {
    let names: Vec<&str> = connectors.map(|c| c.endpoint.as_str()).take(10).collect();
    names.join(", ")
}

/// The connector and tool a call of the standard tool `name` goes to, with the arguments it is
/// sent: without [`ENDPOINT`] and, for an unfiltered `query_entities`, a [`SAMPLE_ROWS`] sample
/// with the total. A refusal says what to correct.
pub fn resolved<'a>(
    connectors: &'a [Connected],
    name: &str,
    mut arguments: Value,
) -> Result<(&'a Connected, &'a Tool, Value), String> {
    if !READ_ONLY_TOOLS.contains(&name)
        || !connectors
            .iter()
            .any(|c| c.tools.iter().any(|t| t.name == name))
    {
        return Err("There is no tool of that name. Use only the tools you were given.".into());
    }
    let named = match arguments.as_object_mut().and_then(|a| a.remove(ENDPOINT)) {
        Some(Value::String(named)) if !named.trim().is_empty() => named,
        _ => {
            return Err(format!(
                "The call was not made: `endpoint` is required; name one of: {}.",
                names_of(connectors.iter())
            ))
        }
    };
    let connector =
        endpoint_of(connectors, &named).map_err(|why| format!("The call was not made: {why}."))?;
    let Some(tool) = connector.tools.iter().find(|t| t.name == name) else {
        let has: Vec<&str> = connector.tools.iter().map(|t| t.name.as_str()).collect();
        return Err(format!(
            "The call was not made: Endpoint {} offers no {name}; it offers {}.",
            connector.endpoint,
            has.join(", ")
        ));
    };
    if name == "query_entities" && !FILTERS.iter().any(|f| arguments.get(*f).is_some()) {
        let asked = arguments
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(u64::MAX);
        arguments["limit"] = json!(asked.min(SAMPLE_ROWS));
        if tool.input_schema.pointer("/properties/count").is_some() {
            arguments["count"] = json!(true);
        }
    }
    if let Err(why) = fits(&tool.input_schema, &arguments) {
        return Err(format!(
            "The call was not made: {why}. Correct the arguments, or answer without this tool."
        ));
    }
    Ok((connector, tool, arguments))
}

/// Data for the model: `<` and `>` escaped, so an earlier turn cannot close its own block.
pub fn quoted(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Whether `arguments` satisfy `schema`; else where and which rule they break, never the value,
/// which goes back to the model. A schema that does not compile admits nothing.
pub fn fits(schema: &Value, arguments: &Value) -> Result<(), String> {
    let validator = jsonschema::validator_for(schema)
        .map_err(|_| "the tool's schema cannot be read, so no call is made to it".to_owned())?;
    let problems: Vec<String> = validator
        .iter_errors(arguments)
        .take(5)
        .map(|error| {
            let rule = match error.kind() {
                jsonschema::error::ValidationErrorKind::Required { property } => {
                    format!("{} is required", property.as_str().unwrap_or("an argument"))
                }
                jsonschema::error::ValidationErrorKind::AdditionalProperties { unexpected } => {
                    format!("no argument is called {}", unexpected.join(", "))
                }
                kind => format!("breaks the schema's {}", kind.keyword()),
            };
            match error.instance_path().to_string() {
                at if at.is_empty() => rule,
                at => format!("{at}: {rule}"),
            }
        })
        .collect();
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems.join("; "))
    }
}
