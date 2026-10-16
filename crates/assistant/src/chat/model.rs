//! The model, reached only through `jc-agent-proxy` (ADR-N-040 §3.4, AG-109…AG-111): the
//! service's own client-credentials token, the deployment named in a header, one OpenAI-compatible
//! chat completion per call. The service holds no model key.

use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::sync::mpsc::Sender;
use tokio::sync::Mutex;

use super::agent::Event;

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
    /// What the provider says the call cost, in millionths of a US dollar (OpenRouter's
    /// `usage.cost`, T-4149); `None` when it says nothing, which is not the same as free (T-4178).
    pub cost_micros: Option<u64>,
}

/// `usage.cost` in millionths of a US dollar; `None` when absent, negative or not a number.
fn cost_micros(usage: &Value) -> Option<u64> {
    let cost = usage.get("cost")?.as_f64()?;
    (cost.is_finite() && cost >= 0.0).then(|| (cost * 1_000_000.0).round() as u64)
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
    /// How it proves that client: the pod's projected token, or a secret read from the file the
    /// deployment mounts (PF-47).
    pub credential: jc_core::ClientCredential,
    /// The model the completions name.
    pub model: String,
}

/// Calls the model; holds the service's token until shortly before it expires.
pub struct Model {
    config: ModelConfig,
    http: reqwest::Client,
    token: Mutex<Option<(String, Instant)>>,
}

/// The longest answer one call may write. A reasoning model counts its thinking inside it, so
/// `REASONING_EFFORT` keeps the thinking short and the room is the answer's (T-3203).
pub const MAX_ANSWER_TOKENS: u64 = 3_000;

/// The reasoning effort every call asks for, which the proxy passes on as it is. Left to the
/// provider, gemini-3.8-flash thought through 1,438 of 1,500 tokens and the last call of a
/// question came back empty: "No Answer" on dev (T-3203).
const REASONING_EFFORT: &str = "low";

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
            CallError::Unavailable(MODEL_UNREACHABLE.into())
        };
        let mut form = self
            .config
            .credential
            .form(&self.config.client_id)
            .map_err(unavailable)?;
        form.push(("grant_type", "client_credentials".to_owned()));
        let answer = self
            .http
            .post(&self.config.token_url)
            .form(&form)
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

    /// Whether `bearer` is an active token of `client`'s own service account whose audience names
    /// this service (AG-113): the realm is asked with the service's own credentials, so a token
    /// it revoked is refused at once.
    pub async fn is_service_account(&self, bearer: &str, client: &str) -> Result<bool, CallError> {
        let unavailable = |why: String| {
            tracing::error!(%why, "a caller's token could not be judged");
            CallError::Unavailable("The realm could not be reached; try again shortly.".into())
        };
        let mut form = self
            .config
            .credential
            .form(&self.config.client_id)
            .map_err(unavailable)?;
        form.push(("token", bearer.to_owned()));
        let answer = self
            .http
            .post(format!("{}/introspect", self.config.token_url))
            .form(&form)
            .send()
            .await
            .map_err(|err| unavailable(err.without_url().to_string()))?;
        if !answer.status().is_success() {
            return Err(unavailable(format!(
                "introspection answered {}",
                answer.status()
            )));
        }
        let seen: Value = answer
            .json()
            .await
            .map_err(|err| unavailable(err.without_url().to_string()))?;
        let audience = match seen.get("aud") {
            Some(Value::String(one)) => vec![one.as_str()],
            Some(Value::Array(many)) => many.iter().filter_map(Value::as_str).collect(),
            _ => Vec::new(),
        };
        Ok(seen.get("active").and_then(Value::as_bool) == Some(true)
            && seen.get("azp").and_then(Value::as_str) == Some(client)
            && seen
                .get("username")
                .and_then(Value::as_str)
                .is_some_and(|u| u.eq_ignore_ascii_case(&format!("service-account-{client}")))
            && audience.contains(&self.config.client_id.as_str()))
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
        let mut body = json!({
            "model": self.config.model,
            "messages": messages,
            "max_tokens": MAX_ANSWER_TOKENS,
            "reasoning": {"effort": REASONING_EFFORT},
        });
        if !tools.is_empty() {
            body["tools"] = Value::Array(tools.to_vec());
            if !allow_tools {
                body["tool_choice"] = json!("none");
            }
        }
        let answer = self.send(deployment, tokens_per_day, &body).await?;
        let body: Value = answer.json().await.unwrap_or(Value::Null);
        parse(&body).ok_or_else(|| {
            unavailable(deployment, "the provider's answer is not a chat completion")
        })
    }

    /// One call without tools on `call.model` (the service's model when `None`), its output held
    /// to `call.max_tokens`; a router call asks for a JSON object (T-4139, AG-107).
    pub async fn ask(
        &self,
        deployment: &str,
        tokens_per_day: u64,
        call: &Call<'_>,
        messages: &[Value],
    ) -> Result<Completion, CallError> {
        let body = self.body(call, messages, false);
        let answer = self.send(deployment, tokens_per_day, &body).await?;
        let body: Value = answer.json().await.unwrap_or(Value::Null);
        parse(&body).ok_or_else(|| {
            unavailable(deployment, "the provider's answer is not a chat completion")
        })
    }

    /// One step of the data loop (T-4294): [`Model::ask`]'s call with `tools` offered, or shown
    /// with `tool_choice: none` when `allow_tools` is false, so a history of tool calls stays valid.
    pub async fn step(
        &self,
        deployment: &str,
        tokens_per_day: u64,
        call: &Call<'_>,
        messages: &[Value],
        tools: &[Value],
        allow_tools: bool,
    ) -> Result<Completion, CallError> {
        let mut body = self.body(call, messages, false);
        if !tools.is_empty() {
            body["tools"] = Value::Array(tools.to_vec());
            if !allow_tools {
                body["tool_choice"] = json!("none");
            }
        }
        let answer = self.send(deployment, tokens_per_day, &body).await?;
        let body: Value = answer.json().await.unwrap_or(Value::Null);
        parse(&body).ok_or_else(|| {
            unavailable(deployment, "the provider's answer is not a chat completion")
        })
    }

    /// The same call streamed: each piece of the answer goes to `deltas` as a `delta` event the
    /// moment the model writes it, and the whole completion, with the stream's `usage`, comes back
    /// at the end with the instant its first piece arrived (API/05 §1.3).
    pub async fn stream(
        &self,
        deployment: &str,
        tokens_per_day: u64,
        call: &Call<'_>,
        messages: &[Value],
        deltas: &Sender<Event>,
    ) -> Result<(Completion, Option<Instant>), CallError> {
        let body = self.body(call, messages, true);
        let answer = self.send(deployment, tokens_per_day, &body).await?;
        let mut reader = StreamReader::default();
        let mut first = None;
        let mut bytes = answer.bytes_stream();
        while let Some(chunk) = futures_util::StreamExt::next(&mut bytes).await {
            let chunk =
                chunk.map_err(|err| unavailable(deployment, &err.without_url().to_string()))?;
            for piece in reader.feed(&chunk) {
                first.get_or_insert_with(Instant::now);
                // A reader who left stops reading; the call is let finish so it is counted.
                let _ = deltas.send(Event::Delta(piece)).await;
            }
        }
        let completion = reader.finish().ok_or_else(|| {
            unavailable(deployment, "the provider's stream ended without an answer")
        })?;
        Ok((completion, first))
    }

    fn body(&self, call: &Call<'_>, messages: &[Value], stream: bool) -> Value {
        let mut body = json!({
            "model": call.model.unwrap_or(&self.config.model),
            "messages": messages,
            "max_tokens": call.max_tokens,
            "reasoning": {"effort": REASONING_EFFORT},
        });
        if call.json {
            body["response_format"] = json!({"type": "json_object"});
        }
        if stream {
            // The proxy adds `stream_options.include_usage`, so the last chunk carries `usage`.
            body["stream"] = json!(true);
        }
        body
    }

    /// Sends one chat completion through the proxy; a refusal becomes the sentence a person reads.
    async fn send(
        &self,
        deployment: &str,
        tokens_per_day: u64,
        body: &Value,
    ) -> Result<reqwest::Response, CallError> {
        let token = self.token().await?;
        let answer = self
            .http
            .post(format!("{}/v1/llm/chat/completions", self.config.proxy))
            .bearer_auth(token)
            .header("x-jc-assistant-deployment", deployment)
            .header("x-jc-assistant-tokens-per-day", tokens_per_day.to_string())
            .json(body)
            .send()
            .await
            .map_err(|err| unavailable(deployment, &err.without_url().to_string()))?;
        let status = answer.status();
        if status.as_u16() == 429 {
            return Err(CallError::DaySpent(DAY_SPENT.into()));
        }
        if status.as_u16() == 401 {
            // The held token was refused: the next question asks the realm again.
            *self.token.lock().await = None;
        }
        if !status.is_success() {
            return Err(unavailable(
                deployment,
                &format!("the proxy answered {status}"),
            ));
        }
        Ok(answer)
    }
}

// What a visitor reads when the model fails (T-4192): in Slovak, like every sentence the public
// chat says itself (the 504, the switched-off assistant); the status and title stay English.
pub const MODEL_UNREACHABLE: &str =
    "Asistent sa teraz nevie spojiť so svojím modelom. Skúste to o minútu znova.";
pub const MODEL_DID_NOT_ANSWER: &str = "Model asistenta neodpovedal. Skúste to o minútu znova.";
/// Why the proxy refused, for the log: a visitor never reads it, the chat answers as search
/// instead (T-4286).
pub const DAY_SPENT: &str = "the model proxy refused the deployment's day (budget.tokensPerDay)";
pub const CONVERSATION_SPENT: &str =
    "Tento rozhovor už minul svoj podiel asistenta. Ak sa chcete pýtať ďalej, začnite nový rozhovor.";

fn unavailable(deployment: &str, why: &str) -> CallError {
    tracing::warn!(%deployment, %why, "a model call failed");
    CallError::Unavailable(MODEL_DID_NOT_ANSWER.into())
}

/// What one call of [`Model::ask`] or [`Model::stream`] asks for.
#[derive(Debug, Clone, Copy)]
pub struct Call<'a> {
    /// `vendor/name`; the service's configured model when `None`.
    pub model: Option<&'a str>,
    /// The most tokens the call may write, its thinking included.
    pub max_tokens: u64,
    /// Whether the answer must be one JSON object.
    pub json: bool,
}

/// Reads an OpenAI-compatible completion stream (`data: {chunk}` lines, `data: [DONE]` last) as
/// its bytes arrive, in pieces that may cut a line, or a character, anywhere.
#[derive(Debug, Default)]
pub struct StreamReader {
    pending: Vec<u8>,
    text: String,
    input: u64,
    output: u64,
    total: Option<u64>,
    cost_micros: Option<u64>,
    chunks: usize,
}

/// The longest line the reader keeps; a longer one is dropped unread.
const STREAM_LINE_MAX: usize = 1 << 20;

impl StreamReader {
    /// Takes the next bytes; returns the answer's pieces they complete, in order.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<String> {
        self.pending.extend_from_slice(bytes);
        let mut pieces = Vec::new();
        while let Some(end) = self.pending.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.pending.drain(..=end).collect();
            if let Some(piece) = self.line(&line) {
                pieces.push(piece);
            }
        }
        if self.pending.len() > STREAM_LINE_MAX {
            self.pending.clear();
        }
        pieces
    }

    fn line(&mut self, line: &[u8]) -> Option<String> {
        let line = std::str::from_utf8(line).ok()?.trim();
        let data = line.strip_prefix("data:")?.trim();
        if data == "[DONE]" {
            return None;
        }
        let chunk: Value = serde_json::from_str(data).ok()?;
        self.chunks += 1;
        if let Some(usage) = chunk.get("usage").filter(|u| u.is_object()) {
            let count = |key: &str| usage.get(key).and_then(Value::as_u64);
            self.input = count("prompt_tokens").unwrap_or(self.input);
            self.output = count("completion_tokens").unwrap_or(self.output);
            self.total = count("total_tokens").or(self.total);
            self.cost_micros = cost_micros(usage).or(self.cost_micros);
        }
        let piece = chunk.pointer("/choices/0/delta/content")?.as_str()?;
        if piece.is_empty() {
            return None;
        }
        self.text.push_str(piece);
        Some(piece.to_owned())
    }

    /// The whole completion, or `None` when the stream carried no chunk at all.
    pub fn finish(mut self) -> Option<Completion> {
        if !self.pending.is_empty() {
            let rest = std::mem::take(&mut self.pending);
            self.line(&rest);
        }
        (self.chunks > 0).then(|| Completion {
            tokens: self.total.unwrap_or(self.input + self.output),
            text: self.text,
            calls: Vec::new(),
            input: self.input,
            output: self.output,
            cost_micros: self.cost_micros,
        })
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
        cost_micros: usage.and_then(cost_micros),
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

    /// T-4149, T-4178: the provider's `usage.cost` is read in millionths of a dollar, from a
    /// whole completion and from a stream's last chunk; a cost that is absent, negative or not a
    /// number is `None`, never a free call.
    #[test]
    fn a_call_s_cost_is_read_from_its_usage() {
        let with = |cost: Value| {
            parse(&json!({
                "choices": [{"message": {"role": "assistant", "content": "a"}}],
                "usage": {"prompt_tokens": 9, "completion_tokens": 1, "cost": cost}
            }))
            .expect("a completion")
            .cost_micros
        };
        assert_eq!(with(json!(0.0042)), Some(4_200));
        assert_eq!(with(json!(3)), Some(3_000_000));
        assert_eq!(with(json!(-1.0)), None);
        assert_eq!(with(json!("lots")), None);
        assert_eq!(with(Value::Null), None);
        let without = parse(&json!({
            "choices": [{"message": {"role": "assistant", "content": "a"}}],
            "usage": {"prompt_tokens": 9, "completion_tokens": 1}
        }));
        assert_eq!(without.expect("a completion").cost_micros, None);
        let mut reader = StreamReader::default();
        reader.feed(concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"a\"}}]}\n\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":1,\"cost\":0.000125}}\n\n",
        ).as_bytes());
        assert_eq!(
            reader.finish().expect("a completion").cost_micros,
            Some(125)
        );
        let mut silent = StreamReader::default();
        silent.feed(
            concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"a\"}}]}\n\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":1}}\n\n",
        )
            .as_bytes(),
        );
        assert_eq!(silent.finish().expect("a completion").cost_micros, None);
    }

    /// T-4139: the pieces come out as they complete, wherever the bytes were cut, and the usage
    /// of the last chunk is what the call cost.
    #[test]
    fn a_stream_is_read_in_pieces_cut_anywhere() {
        let stream = concat!(
            ": keep-alive\n\n",
            "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"Knižnica \"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"je otvorená [1].\"}}]}\n\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":1200,\"completion_tokens\":40,\"total_tokens\":1240}}\n\n",
            "data: [DONE]\n\n",
        )
        .as_bytes();
        for cut in [1, 7, 33, 100] {
            let mut reader = StreamReader::default();
            let mut pieces = Vec::new();
            for bytes in stream.chunks(cut) {
                pieces.extend(reader.feed(bytes));
            }
            assert_eq!(
                pieces,
                ["Knižnica ", "je otvorená [1]."],
                "cut every {cut} bytes"
            );
            let completion = reader.finish().expect("a completion");
            assert_eq!(completion.text, "Knižnica je otvorená [1].");
            assert_eq!(
                (completion.tokens, completion.input, completion.output),
                (1240, 1200, 40)
            );
        }
        // A stream with no chunk at all is no completion; one without usage counts nothing.
        assert_eq!(StreamReader::default().finish(), None);
        let mut reader = StreamReader::default();
        reader.feed(b"data: {\"choices\":[{\"delta\":{\"content\":\"a\"}}]}");
        assert_eq!(
            reader.finish().map(|c| (c.text, c.tokens)),
            Some(("a".into(), 0))
        );
    }
}
