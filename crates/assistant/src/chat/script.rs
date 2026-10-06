//! A script the model writes over a large tool result, run in `jc-functions` with no network
//! (AG-112, T-3056, ADR-N-040 §3.6): the data goes in as the request body, the script's return
//! value comes back, and nothing else crosses.

use serde_json::{json, Value};

/// The longest script the model may write, in characters.
pub const MAX_CODE_CHARS: usize = 4_000;

/// The code of the one file `jc-functions` runs: the model's body inside an async function of
/// `data`, its return value as the answer's body. Escaping the wrapper gains nothing: the whole
/// file runs in the same sandbox.
pub fn file(code: &str) -> String {
    format!(
        "export default async (request) => {{\n  const data = request.body;\n  const result = await (async () => {{\n{code}\n  }})();\n  return {{ body: result === undefined ? null : result }};\n}};\n"
    )
}

/// The data a result is handed in as: its JSON when it is JSON, its text otherwise.
pub fn data_of(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.to_owned()))
}

/// Runs `code` over `data` in `jc-functions` at `functions` with the service's `token`; the
/// script's output as text, or why there is none, in words for the model and the person.
pub async fn run(
    http: &reqwest::Client,
    functions: &str,
    token: &str,
    code: &str,
    data: Value,
) -> Result<String, String> {
    let body = json!({
        "files": { "script.js": file(code) },
        "entry": "script.js",
        "request": { "method": "POST", "query": {}, "body": data, "user": null },
        "config": {},
        "via": "none",
    });
    let answer = http
        .post(format!("{functions}/invoke"))
        .bearer_auth(token)
        .json(&body)
        .send()
        .await
        .map_err(|err| format!("the sandbox could not be reached: {}", err.without_url()))?;
    let status = answer.status();
    let outcome: Value = answer.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        let detail = outcome
            .get("detail")
            .and_then(Value::as_str)
            .unwrap_or("no reason given");
        return Err(format!(
            "the sandbox refused the script ({status}): {detail}"
        ));
    }
    if let Some(message) = outcome.pointer("/error/message").and_then(Value::as_str) {
        return Err(format!("the script failed: {message}"));
    }
    Ok(match outcome.get("body") {
        Some(Value::String(text)) => text.clone(),
        Some(value) => value.to_string(),
        None => "null".to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_script_is_the_body_of_a_function_of_data() {
        let file = file("return data.length;");
        assert!(file.starts_with("export default async (request) => {"));
        assert!(file.contains("const data = request.body;"));
        assert!(file.contains("return data.length;"));
        assert_eq!(data_of("[1,2]"), json!([1, 2]));
        assert_eq!(data_of("plain text"), json!("plain text"));
    }
}
