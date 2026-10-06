//! The model, reached only through `jc-agent-proxy` (ADR-N-040 §3.4, AG-109…AG-111): the
//! service's own client-credentials token, the deployment named in a header, one OpenAI-compatible
//! chat completion per call. The service holds no model key.

use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::sync::Mutex;

/// What a model call came back with.
#[derive(Debug, Clone, PartialEq)]
pub struct Completion {
    /// The answer's text, empty when the model only asked for tools.
    pub text: String,
    /// The tools it asked for: id, name and the arguments as it wrote them.
    pub calls: Vec<(String, String, String)>,
    /// The tokens the call cost, as the provider counted them: in total, read and written.
    pub tokens: u64,
    pub input: u64,
    pub output: u64,
}

/// Why a call gave no completion, in words a person reads (AG-104).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallError {
    /// The deployment's day is spent (the proxy's `429`).
    DaySpent(String),
    /// The proxy, the realm or the provider failed or answered something unreadable.
    Unavailable(String),
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CallError::DaySpent(why) | CallError::Unavailable(why) => f.write_str(why),
        }
    }
}

/// How the service reaches the model.
#[derive(Debug, Clone)]
pub struct ModelConfig {
    /// `jc-agent-proxy`, scheme and authority.
    pub proxy: String,
    /// The realm's token endpoint.
    pub token_url: String,
    /// The service's Keycloak client.
    pub client_id: String,
    /// Its secret, read from the file the deployment mounts.
    pub client_secret: String,
    /// The model the completions name.
    pub model: String,
}

/// Calls the model; holds the service's token until shortly before it expires.
pub struct Model {
    config: ModelConfig,
    http: reqwest::Client,
    token: Mutex<Option<(String, Instant)>>,
}

/// The longest answer one call may write.
const MAX_ANSWER_TOKENS: u64 = 1_500;

impl Model {
    pub fn new(config: ModelConfig, http: reqwest::Client) -> Self {
        Self {
            config,
            http,
            token: Mutex::new(None),
        }
    }

    /// The service's own token, held until shortly before it expires; its audience names the
    /// agent proxy and `jc-functions`.
    pub async fn token(&self) -> Result<String, CallError> {
        let mut held = self.token.lock().await;
        if let Some((token, until)) = held.as_ref() {
            if Instant::now() < *until {
                return Ok(token.clone());
            }
        }
        let unavailable = |why: String| {
            tracing::error!(%why, "no service token for the model");
            CallError::Unavailable(
                "The assistant cannot reach its model right now. Try again in a minute.".into(),
            )
        };
        let answer = self
            .http
            .post(&self.config.token_url)
            .form(&[
                ("grant_type", "client_credentials"),
                ("client_id", self.config.client_id.as_str()),
                ("client_secret", self.config.client_secret.as_str()),
            ])
            .send()
            .await
            .map_err(|err| unavailable(err.without_url().to_string()))?;
        if !answer.status().is_success() {
            return Err(unavailable(format!(
                "the realm answered {}",
                answer.status()
            )));
        }
        let body: Value = answer
            .json()
            .await
            .map_err(|err| unavailable(err.without_url().to_string()))?;
        let token = body
            .get("access_token")
            .and_then(Value::as_str)
            .ok_or_else(|| unavailable("the realm's answer has no access_token".into()))?
            .to_owned();
        let lives = body.get("expires_in").and_then(Value::as_u64).unwrap_or(60);
        // Renewed a minute early, or at half its life when it lives less than two minutes.
        let keep = if lives > 120 { lives - 60 } else { lives / 2 };
        *held = Some((token.clone(), Instant::now() + Duration::from_secs(keep)));
        Ok(token)
    }

    /// One completion for `deployment` (`{project}/{name}`) under its day's `tokens_per_day`.
    pub async fn complete(
        &self,
        deployment: &str,
        tokens_per_day: u64,
        messages: &[Value],
        tools: &[Value],
        allow_tools: bool,
    ) -> Result<Completion, CallError> {
        let token = self.token().await?;
        let mut body = json!({
            "model": self.config.model,
            "messages": messages,
            "max_tokens": MAX_ANSWER_TOKENS,
        });
        if !tools.is_empty() {
            body["tools"] = Value::Array(tools.to_vec());
            if !allow_tools {
                body["tool_choice"] = json!("none");
            }
        }
        let unavailable = |why: String| {
            tracing::warn!(%deployment, %why, "a model call failed");
            CallError::Unavailable(
                "The assistant's model did not answer. Try again in a minute.".into(),
            )
        };
        let answer = self
            .http
            .post(format!("{}/v1/llm/v1/chat/completions", self.config.proxy))
            .bearer_auth(token)
            .header("x-jc-assistant-deployment", deployment)
            .header("x-jc-assistant-tokens-per-day", tokens_per_day.to_string())
            .json(&body)
            .send()
            .await
            .map_err(|err| unavailable(err.without_url().to_string()))?;
        let status = answer.status();
        let body: Value = answer.json().await.unwrap_or(Value::Null);
        if status.as_u16() == 429 {
            return Err(CallError::DaySpent(
                "Today's answers for this assistant are used up. It answers again after midnight UTC."
                    .into(),
            ));
        }
        if status.as_u16() == 401 {
            // The held token was refused: the next question asks the realm again.
            *self.token.lock().await = None;
        }
        if !status.is_success() {
            return Err(unavailable(format!("the proxy answered {status}")));
        }
        parse(&body)
            .ok_or_else(|| unavailable("the provider's answer is not a chat completion".into()))
    }
}

/// The text, the tool calls and the tokens of an OpenAI-compatible chat completion.
pub fn parse(body: &Value) -> Option<Completion> {
    let message = body.pointer("/choices/0/message")?;
    let text = message
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let calls = message
        .get("tool_calls")
        .and_then(Value::as_array)
        .map(|calls| {
            calls
                .iter()
                .filter_map(|call| {
                    Some((
                        call.get("id")?.as_str()?.to_owned(),
                        call.pointer("/function/name")?.as_str()?.to_owned(),
                        call.pointer("/function/arguments")
                            .and_then(Value::as_str)
                            .unwrap_or("{}")
                            .to_owned(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default();
    let usage = body.get("usage");
    let count = |key: &str| usage.and_then(|u| u.get(key)).and_then(Value::as_u64);
    let input = count("prompt_tokens").unwrap_or(0);
    let output = count("completion_tokens").unwrap_or(0);
    Some(Completion {
        text,
        calls,
        tokens: count("total_tokens").unwrap_or(input + output),
        input,
        output,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_completion_reads_its_text_its_tool_calls_and_its_tokens() {
        let body = json!({
            "choices": [{"message": {"role": "assistant", "content": null, "tool_calls": [
                {"id": "c1", "type": "function", "function": {"name": "search", "arguments": "{\"query\":\"knižnica\"}"}},
                {"id": "c2", "type": "function", "function": {"name": "x"}},
                {"type": "function", "function": {"name": "no-id"}}
            ]}}],
            "usage": {"prompt_tokens": 90, "completion_tokens": 10}
        });
        let completion = parse(&body).expect("a completion");
        assert_eq!(completion.text, "");
        assert_eq!(
            completion.calls,
            vec![
                (
                    "c1".into(),
                    "search".into(),
                    "{\"query\":\"knižnica\"}".into()
                ),
                ("c2".into(), "x".into(), "{}".into())
            ]
        );
        assert_eq!(
            (completion.tokens, completion.input, completion.output),
            (100, 90, 10)
        );
        assert_eq!(parse(&json!({"error": "x"})), None);
    }
}
