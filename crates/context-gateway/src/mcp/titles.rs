//! The human title of each listed tool (T-4732).

use serde_json::{json, Value};

/// Every listed tool with a human title, as `title` and `annotations.title` (MCP 2025-06-18):
/// a directory of connectors refuses a tool without one (T-4732). A tool keeps a title it
/// already has; otherwise its name in words, `query_entities` as "Query entities". The name
/// itself never changes, since clients call tools by it.
pub(crate) fn titled(tools: &mut [Value]) {
    for tool in tools.iter_mut().filter_map(Value::as_object_mut) {
        let Some(name) = tool.get("name").and_then(Value::as_str) else {
            continue;
        };
        let title = match tool.get("title").and_then(Value::as_str) {
            Some(own) if !own.trim().is_empty() => own.to_owned(),
            _ => in_words(name),
        };
        if !tool.get("annotations").is_some_and(Value::is_object) {
            tool.insert("annotations".to_owned(), json!({}));
        }
        if let Some(annotations) = tool.get_mut("annotations").and_then(Value::as_object_mut) {
            annotations.insert("title".to_owned(), json!(title));
        }
        tool.insert("title".to_owned(), json!(title));
    }
}

/// `query_entities` as "Query entities".
fn in_words(name: &str) -> String {
    let words = name.replace('_', " ");
    let mut chars = words.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(chars).collect()
    })
}
