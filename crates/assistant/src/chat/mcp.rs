//! A connector's tools: the MCP surface of one Endpoint on the Context Gateway,
//! `POST /api/endpoint/{slug}/mcp`, one JSON-RPC message per request (EP-37, AG-106). On the
//! anonymous channels it is called without a token, so the gateway answers as it answers anyone;
//! an `internal` deployment's non-public Endpoint is called with the asking person's own token
//! (API/05 §1.7, AG-115).

use std::time::Duration;

use serde_json::{json, Value};

/// The most of one tool result the model reads; the rest is cut and the cut is said.
pub const MAX_RESULT_CHARS: usize = 20_000;

/// The most of one tool result kept for a script to read (AG-112): `jc-functions` takes a
/// request body of 256 KiB.
pub const MAX_KEPT_CHARS: usize = 250_000;

/// One tool of a connector, as the model is offered it.
#[derive(Debug, Clone, PartialEq)]
pub struct Tool {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

/// A person's access token, for the one question it came with: never printed, never stored.
#[derive(Clone)]
pub struct PersonToken(pub String);

impl std::fmt::Debug for PersonToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PersonToken(..)")
    }
}

/// The MCP surface of one Endpoint.
#[derive(Debug, Clone)]
pub struct Surface {
    /// The gateway, scheme and authority.
    pub gateway: String,
    pub slug: String,
    pub timeout: Duration,
    /// The person the call reads as; `None` calls as anyone.
    pub token: Option<PersonToken>,
}

impl Surface {
    async fn rpc(
        &self,
        http: &reqwest::Client,
        method: &str,
        params: Value,
    ) -> Result<Value, String> {
        let mut request = http
            .post(format!("{}/api/endpoint/{}/mcp", self.gateway, self.slug))
            .timeout(self.timeout)
            .json(&json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}));
        if let Some(token) = &self.token {
            request = request.bearer_auth(&token.0);
        }
        let answer = request
            .send()
            .await
            .map_err(|err| err.without_url().to_string())?;
        let status = answer.status();
        if !status.is_success() {
            return Err(format!("the Endpoint answered {status}"));
        }
        let body: Value = answer
            .json()
            .await
            .map_err(|err| format!("the Endpoint's answer is not JSON: {}", err.without_url()))?;
        if let Some(error) = body.get("error") {
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("an error");
            return Err(format!("the Endpoint refused: {message}"));
        }
        body.get("result")
            .cloned()
            .ok_or_else(|| "the Endpoint's answer has no result".to_owned())
    }

    /// The tools the surface offers that `allowed` names, in the order `allowed` names them.
    pub async fn tools(
        &self,
        http: &reqwest::Client,
        allowed: &[String],
    ) -> Result<Vec<Tool>, String> {
        let result = self.rpc(http, "tools/list", json!({})).await?;
        let listed = result
            .get("tools")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        Ok(allowed
            .iter()
            .filter_map(|name| {
                let tool = listed
                    .iter()
                    .find(|tool| tool.get("name").and_then(Value::as_str) == Some(name))?;
                Some(Tool {
                    name: name.clone(),
                    description: tool
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    input_schema: tool
                        .get("inputSchema")
                        .cloned()
                        .unwrap_or_else(|| json!({"type": "object"})),
                })
            })
            .collect())
    }

    /// Calls `name` with `arguments` and returns its text content, cut at [`MAX_KEPT_CHARS`].
    pub async fn call(
        &self,
        http: &reqwest::Client,
        name: &str,
        arguments: Value,
    ) -> Result<String, String> {
        let result = self
            .rpc(
                http,
                "tools/call",
                json!({"name": name, "arguments": arguments}),
            )
            .await?;
        let text: Vec<&str> = result
            .get("content")
            .and_then(Value::as_array)
            .map(|parts| {
                parts
                    .iter()
                    .filter(|part| part.get("type").and_then(Value::as_str) == Some("text"))
                    .filter_map(|part| part.get("text").and_then(Value::as_str))
                    .collect()
            })
            .unwrap_or_default();
        let text = text.join("\n");
        if result.get("isError").and_then(Value::as_bool) == Some(true) {
            return Err(format!("the tool failed: {}", cut(&text, 500)));
        }
        Ok(cut(&text, MAX_KEPT_CHARS))
    }
}

/// `text` cut at `max` characters, saying so.
pub fn cut(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        None => text.to_owned(),
        Some((at, _)) => format!(
            "{}\n[cut: the result went on past {max} characters]",
            &text[..at]
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::cut;

    #[test]
    fn a_long_result_is_cut_on_a_character_and_says_so() {
        assert_eq!(cut("ľščťž", 10), "ľščťž");
        let cut_text = cut("ľščťž", 2);
        assert!(cut_text.starts_with("ľš\n[cut:"));
        assert!(cut_text.contains("2 characters"));
    }
}
